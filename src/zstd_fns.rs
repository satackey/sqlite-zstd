use rand::Rng;
use rusqlite::functions::Context;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ToSqlOutput;
use rusqlite::types::{Null, ToSql};
use rusqlite::types::{Value, ValueRef};
use rusqlite::Error::UserFunctionError as UFE;
use rusqlite::{named_params, params};
use serde_json::json;
use std::borrow::Cow;
use std::convert::TryInto;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use zstd::dict::EncoderDictionary;

/*fn to_rusqlite<E>(e: Box<E>) -> rusqlite::Error
where
    E: std::error::Error + std::marker::Send + std::marker::Sync,
{
    rusqlite::Error::UserFunctionError(e)
}*/

/*fn to_rusqlite<'e>(
    e: impl std::error::Error + std::marker::Send + std::marker::Sync + 'e,
) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(Box::new(e))
}*/

struct ZstdTrainDictAggregate;
struct ZstdTrainDictState {
    reservoir: Vec<Vec<u8>>,
    wanted_item_count: usize,
    total_count: usize,
    wanted_dict_size: usize,
}
impl rusqlite::functions::Aggregate<Option<ZstdTrainDictState>, Value> for ZstdTrainDictAggregate {
    fn init(&self) -> Option<ZstdTrainDictState> {
        None
    }
    fn step(
        &self,
        ctx: &mut Context,
        state: &mut Option<ZstdTrainDictState>,
    ) -> rusqlite::Result<()> {
        if let None = state {
            std::mem::replace(
                state,
                Some(ZstdTrainDictState {
                    reservoir: vec![],
                    wanted_item_count: ctx.get::<f64>(2)? as usize,
                    wanted_dict_size: ctx.get::<i64>(1)? as usize,
                    total_count: 0,
                }),
            );
        }
        let mut state = state.as_mut().unwrap();
        let cur = match ctx.get_raw(0) {
            ValueRef::Blob(b) => b,
            ValueRef::Text(b) => b,
            ValueRef::Real(f) => return Ok(()),
            ValueRef::Integer(i) => return Ok(()),
            ValueRef::Null => return Ok(()),
        };
        let i = state.total_count;
        let k = state.wanted_item_count;
        // https://en.wikipedia.org/wiki/Reservoir_sampling#Simple_algorithm

        if i < k {
            state.reservoir.push(Vec::from(cur));
            state.total_count += 1;
            return Ok(());
        }
        state.total_count += 1;
        let j = rand::thread_rng().gen_range(0, i);
        if j < k {
            state.reservoir[j] = Vec::from(cur);
        }
        Ok(())
    }

    fn finalize(&self, state: Option<Option<ZstdTrainDictState>>) -> rusqlite::Result<Value> {
        if let Some(state) = state.flatten() {
            eprintln!(
                "training dict of size {}kB with {} samples",
                state.wanted_dict_size / 1000,
                state.reservoir.len()
            );
            let dict = zstd::dict::from_samples(&state.reservoir, state.wanted_dict_size)
                .map_err(|e| UFE(Box::new(e)))?;
            Ok(Value::Blob(dict))
        } else {
            Ok(Value::Null)
        }
    }
}

type OwnedEncoderDict<'a> = owning_ref::OwningHandle<Vec<u8>, Box<EncoderDictionary<'a>>>;
// zstd-rs only exposes zstd_safe::create_cdict_by_reference, not zstd_safe::create_cdict
// so we need to keep a reference to the vector ourselves
// is there a better way?
fn wrap_encoder_dict<'a>(dict_raw: Vec<u8>, level: i32) -> OwnedEncoderDict<'a> {
    owning_ref::OwningHandle::new_with_fn(dict_raw, |d| {
        Box::new(EncoderDictionary::new(
            unsafe { d.as_ref() }.unwrap(),
            level,
        ))
    })
}

/// load a dict from sqlite function parameters
///
/// sqlite sadly does not do auxdata caching for subqueries like `zstd_compress(data, 3, (select dict from _zstd_dicts where id = 4))`
/// so instead we support the syntax `zstd_compress(data, 3, 4)` as an alias to the above
/// if the dict parameter is a number, the dict will be queried from the _zstd_dicts table and cached in sqlite auxdata
/// so it is oly constructed once per query
///
/// this function is not 100% correct because the level is passed separately from the dictionary but the dictionary is cached in the aux data of the dictionary parameter
/// e.g. `select zstd_compress(tbl.data, tbl.row_compression_level, 123) from tbl` will probably compress all the data with the same compression ratio instead of a random one
/// as a workaround `select zstd_compress(tbl.data, tbl.row_compression_level, (select 123)) from tbl` probably works
/// to fix this the level parameter would need to be checked against the constructed dictionary and the dict discarded on mismatch
fn encoder_dict_from_ctx<'a>(
    ctx: &'a Context,
    arg_index: usize,
    level: i32,
) -> rusqlite::Result<&'a OwnedEncoderDict<'a>> {
    Ok(match ctx.get_aux::<OwnedEncoderDict>(arg_index as i32)? {
        Some(d) => d,
        None => {
            eprintln!("loading dictionary (should only happen once per query)");
            let dict_raw = /*ctx.get::<Vec<u8>>(arg_index)?;*/
            match ctx.get_raw(arg_index) {
                ValueRef::Blob(b) => b.to_vec(),
                ValueRef::Integer(i) => {
                    let db = ctx.get_connection()?;
                    let res: Vec<u8> = db.query_row(
                        "select dict from _zstd_dicts where id = ?",
                        params![i],
                        |r| r.get(0),
                    )?;
                    res
                }
                e => Err(rusqlite::Error::InvalidFunctionParameterType(
                    arg_index,
                    e.data_type(),
                ))?,
            };
            let dict = wrap_encoder_dict(dict_raw, level);
            ctx.set_aux(arg_index as i32, dict);
            ctx.get_aux::<OwnedEncoderDict>(arg_index as i32)?.unwrap()
        }
    })
}

fn zstd_compress(ctx: &Context) -> Result<Box<dyn ToSql>, rusqlite::Error> {
    let (is_blob, input_value) = match ctx.get_raw(0) {
        ValueRef::Blob(b) => (true, b),
        ValueRef::Text(b) => (false, b),
        // pass through data that is not compressible anyways
        // this is useful because sqlite does not enforce any types so a column of type text or blob can still contain integers etc
        ValueRef::Real(f) => return Ok(Box::new(f)),
        ValueRef::Integer(i) => return Ok(Box::new(i)),
        ValueRef::Null => return Ok(Box::new(Option::<i32>::None)),
    };
    let level = if ctx.len() < 2 {
        // no level given, use default (currently 3)
        0
    } else {
        ctx.get::<i32>(1)?
    };
    let dict = if ctx.len() < 3 {
        // no third argument -> no dict
        None
    } else {
        Some(encoder_dict_from_ctx(&ctx, 2, level)?)
    };

    let is_blob: &[u8] = if is_blob { b"b" } else { b"s" };
    let res = {
        let out = Vec::new();
        let mut encoder = match dict {
            Some(dict) => zstd::stream::write::Encoder::with_prepared_dictionary(out, &dict),
            None => zstd::stream::write::Encoder::new(out, level),
        }
        .map_err(|e| UFE(Box::new(e)))?;
        encoder
            .write_all(input_value)
            .map_err(|e| UFE(Box::new(e)))?;
        encoder.write_all(is_blob).map_err(|e| UFE(Box::new(e)))?;
        encoder.finish()
    };
    Ok(Box::new(res.map_err(|e| UFE(Box::new(e)))?))
}

fn zstd_decompress<'a>(ctx: &Context) -> Result<ToSqlOutput<'a>, rusqlite::Error> {
    let input_value = match ctx.get_raw(0) {
        ValueRef::Blob(b) => b,
        ValueRef::Text(_b) => {
            return Err(UFE(anyhow::anyhow!(
                "got string, but zstd compressed data is always blob"
            )
            .into()))
        }
        ValueRef::Real(f) => return Ok(ToSqlOutput::Owned(Value::Real(f))),
        ValueRef::Integer(i) => return Ok(ToSqlOutput::Owned(Value::Integer(i))),
        ValueRef::Null => return Ok(ToSqlOutput::Owned(Value::Null)),
    };

    let dict_raw = if ctx.len() < 2 {
        // no third argument -> no dict
        None
    } else {
        Some(ctx.get::<Vec<u8>>(1)?)
    };

    let mut vec = {
        let out = Vec::new();
        let mut decoder = match dict_raw {
            Some(dict) => zstd::stream::write::Decoder::with_dictionary(out, &dict),
            None => zstd::stream::write::Decoder::new(out),
        }
        .map_err(|_e| UFE(anyhow::anyhow!("dict load doesn't work").into()))?;
        decoder
            .write_all(input_value)
            .map_err(|e| UFE(Box::new(e)))?;
        decoder.flush().map_err(|e| UFE(Box::new(e)))?;
        decoder.into_inner()
    };

    let is_blob = vec.pop().unwrap();
    if is_blob == b'b' {
        Ok(ToSqlOutput::Owned(Value::Blob(vec)))
    } else {
        Ok(ToSqlOutput::Owned(Value::Text(
            unsafe { String::from_utf8_unchecked(vec) }, // converted right back to &u8 in https://docs.rs/rusqlite/0.21.0/src/rusqlite/types/value_ref.rs.html#107
        )))
    }
    // let dictionary
}

/*fn context_to_db(ctx: &Context) -> Connection {
    let handle = rusqlite::ffi::sqlite3_context_db_handle(ctx.ctx);
}*/

fn debug_row(r: &rusqlite::Row) {
    println!("debugrow");
    let cols = r.column_names();
    for (i, name) in cols.iter().enumerate() {
        print!("{}={}", name, format_blob(r.get_raw(i)))
    }
    println!("");
}

// to dumb for recursive macros
macro_rules! format_sqlite {
    ($x:expr) => {
        format!($x)
    };
    ($x:expr, $y:expr) => {
        format!($x, escape_sqlite_identifier($y))
    };
    ($x:expr, $y:expr, $z:expr) => {
        format!(
            $x,
            escape_sqlite_identifier($y),
            escape_sqlite_identifier($z)
        )
    };
    ($x:expr, $y:expr, $z:expr, $w:expr) => {
        format!(
            $x,
            escape_sqlite_identifier($y),
            escape_sqlite_identifier($z),
            escape_sqlite_identifier($w)
        )
    };
}

///
/// enables transparent row-level compression for a table with the following steps:
///
/// 1. renames tablename to _tablename_zstd
/// 2. trains a zstd dictionary on the column by sampling the existing data in the table
/// 3. creates a view called tablename that mirrors _tablename_zstd except it decompresses the compressed column on the fly
/// 4. creates INSERT, UPDATE and DELETE triggers on the view so they affect the backing table instead
///
fn zstd_enable_transparent<'a>(ctx: &Context) -> Result<ToSqlOutput<'a>, rusqlite::Error> {
    let table_name: String = ctx.get(0)?;
    let new_table_name = format!("_{}_zstd", table_name);
    let column_name: String = ctx.get(1)?;
    let db = &mut ctx.get_connection()?;
    let db = db.transaction()?;

    let columns_info: Vec<(String, bool)> = db
        .prepare(&format_sqlite!(r#"pragma table_info({})"#, &table_name))?
        .query_map(params![], |row| {
            Ok((
                row.get::<&str, String>("name")?,
                row.get::<&str, bool>("pk")?,
            ))
        })?
        .collect::<Result<_, rusqlite::Error>>()?;
    let columns: Vec<String> = columns_info.iter().map(|e| e.0.clone()).collect();
    let primary_key_columns: Vec<String> = columns_info
        .iter()
        .filter(|e| e.1)
        .map(|e| e.0.clone())
        .collect();
    if columns.len() == 0 {
        return Err(UFE(
            anyhow::anyhow!("Table {} does not exist", table_name).into()
        ));
    }
    if !columns.contains(&column_name) {
        return Err(UFE(anyhow::anyhow!(
            "Column {} does not exist in {}",
            column_name,
            table_name
        )
        .into()));
    }
    println!("cols={:?}", columns);
    {
        // can't use prepared statement at these positions
        let rename_query = format_sqlite!(
            r#"alter table {} rename to {}"#,
            &table_name,
            &new_table_name
        );
        eprintln!("[run] {}", &rename_query);
        /*for row in */
        match db.prepare(&rename_query)?.query(params![])?.next()? {
            None => {}
            Some(r) => {
                debug_row(r);
                return Err(UFE(anyhow::anyhow!(
                    "unexpected row returned from alter table"
                )
                .into()));
            }
        }

        db.execute(
            "
            create table if not exists _zstd_dicts (
                id integer not null primary key autoincrement,
                meta json not null,
                dict blob not null
            );",
            params![],
        )?;
    }

    let dict_size = 100000; // 100kB is the default
    let group_by = "null";
    let compression_level = 0; // 0 = default = 3
    let dict_rowid = {
        // we use sample_count = select dict_size * 100 / avg(length(data))
        // because the zstd docs recommend using around 100x the target dictionary size of data to train the dictionary
        let train_query = format_sqlite!("
            insert into _zstd_dicts (meta,dict) values (:meta,
                (select zstd_train_dict({0}, :dict_size, (select :dict_size * 100 / avg(length({0})) as sample_count from {1}))
                    as dict from {1})
            );", &column_name, &new_table_name);
        eprintln!("[run] {}", &train_query);
        db.execute_named(
            &train_query,
            named_params! {
                ":meta": serde_json::to_string(&json!({
                    "tbl": table_name,
                    "col": column_name,
                    "group_by": group_by,
                    "dict_size": dict_size,
                    "created": chrono::Utc::now().to_rfc3339()
                })).map_err(|e| UFE(Box::new(e)))?,
                ":dict_size": dict_size
            },
        )?;
        db.last_insert_rowid()
    };
    {
        let compress_query = format_sqlite!(
            "update {} set {1} = zstd_compress({1}, ?, ?)",
            &new_table_name,
            &column_name
        );
        eprintln!("[run] {}", compress_query);
        let updated = db.execute(&compress_query, params![compression_level, dict_rowid])?;
        eprintln!(
            "compressed {} rows of {}.{}",
            updated, table_name, column_name
        );
    }

    {
        let select_columns_escaped = columns
            .iter()
            .map(|c| {
                if &column_name == c {
                    format!(
                        // prepared statement parameters not allowed in view
                        "zstd_decompress({}, (select dict from _zstd_dicts where id={})) as {0}",
                        escape_sqlite_identifier(&column_name),
                        dict_rowid
                    )
                } else {
                    format_sqlite!("{}", c)
                }
            })
            .collect::<Vec<String>>()
            .join(", ");
        let createview_query = format!(
            r#"
            create view {} as
                select {}
                from {}
            "#,
            escape_sqlite_identifier(&table_name),
            select_columns_escaped,
            escape_sqlite_identifier(&new_table_name)
        );
        eprintln!("[run] {}", &createview_query);
        db.execute(&createview_query, params![])?;
    }

    {
        let select_columns_escaped = columns
            .iter()
            .map(|c| {
                if &column_name == c {
                    format!(
                        // prepared statement parameters not allowed in view
                        "zstd_compress(new.{}, {}, (select dict from _zstd_dicts where id={})) as {0}",
                        escape_sqlite_identifier(&column_name),
                        compression_level,
                        dict_rowid
                    )
                } else {
                    format_sqlite!("new.{}", c)
                }
            })
            .collect::<Vec<String>>()
            .join(", ");
        let createtrigger_query = format!(
            r#"
            create trigger {}
                instead of insert on {}
                begin
                    insert into {} select {};
                end;
            "#,
            escape_sqlite_identifier(&format!("{}_insert_trigger", table_name)),
            escape_sqlite_identifier(&table_name),
            escape_sqlite_identifier(&new_table_name),
            select_columns_escaped
        );
        eprintln!("[run] {}", &createtrigger_query);
        db.execute(&createtrigger_query, params![])?;
    }
    {
        let select_columns_escaped = primary_key_columns
            .iter()
            .map(|c| {
                if &column_name == c {
                    format!(
                        // prepared statement parameters not allowed in view
                        "zstd_compress(old.{}, {}, (select dict from _zstd_dicts where id={})) = {0}",
                        escape_sqlite_identifier(&column_name),
                        compression_level,
                        dict_rowid
                    )
                } else {
                    format_sqlite!("old.{0} = {0}", c)
                }
            })
            .collect::<Vec<String>>()
            .join(" and ");

        let deletetrigger_query = format!(
            r#"
            create trigger {}
                instead of delete on {}
                begin
                    delete from {} where {};
                end;
            "#,
            escape_sqlite_identifier(&format!("{}_delete_trigger", table_name)),
            escape_sqlite_identifier(&table_name),
            escape_sqlite_identifier(&new_table_name),
            select_columns_escaped
        );
        eprintln!("[run] {}", &deletetrigger_query);
        db.execute(&deletetrigger_query, params![])?;
    }
    db.commit()?;
    eprintln!("consider running pragma vacuum to clean up old data");
    Ok(ToSqlOutput::Owned(Value::Text("Done!".to_string())))
}

pub fn add_functions(db: &rusqlite::Connection) -> anyhow::Result<()> {
    let nondeterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY;
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    //
    db.create_scalar_function("zstd_compress", 1, deterministic, zstd_compress)?;
    db.create_scalar_function("zstd_compress", 2, deterministic, zstd_compress)?;
    db.create_scalar_function("zstd_compress", 3, deterministic, zstd_compress)?;
    db.create_scalar_function("zstd_decompress", 1, deterministic, zstd_decompress)?;
    db.create_scalar_function("zstd_decompress", 2, deterministic, zstd_decompress)?;
    db.create_aggregate_function(
        "zstd_train_dict",
        3,
        nondeterministic,
        ZstdTrainDictAggregate,
    )?;
    db.create_scalar_function(
        "zstd_enable_transparent",
        2,
        nondeterministic,
        zstd_enable_transparent,
    )?;
    Ok(())
}

fn format_blob(b: ValueRef) -> String {
    use ValueRef::*;
    match b {
        Null => "NULL".to_owned(),
        Integer(i) => format!("{}", i),
        Real(i) => format!("{}", i),
        Text(i) => format!("'{}'", String::from_utf8_lossy(i).replace("'", "''")),
        Blob(b) => format!("[blob {}B]", b.len()),
    }
}

///
/// adapted from https://github.com/jgallagher/rusqlite/blob/022266239233857faa7f0b415c1a3d5095d96a53/src/vtab/mod.rs#L629
/// sql injection safe? investigate
/// hello -> `hello`
/// he`lo -> `he``lo`
///
/// we intentionally use the `e` syntax instead of "e" because of
/// "a misspelled double-quoted identifier will be interpreted as a string literal, rather than generating an error"
/// see https://www.sqlite.org/quirks.html#double_quoted_string_literals_are_accepted
///
fn escape_sqlite_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace("`", "``"))
}

/**
 * this is needed because _parameters are not allowed in views_, so using prepared statements is not possible
 */
fn escape_sqlite_string(string: &str) -> String {
    format!("'{}'", string.replace("'", "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    // the point of this is that it's something you might store in a DB that has lots of redundant data
    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "type")]
    enum EventData {
        OpenApplication {
            id: i32,
            app_name: String,
            app_type: String,
            properties: BTreeMap<String, String>,
        },
        CloseApplication {
            id: i32,
        },
        Shutdown,
    }

    fn create_example_db() -> anyhow::Result<Connection> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch(
            "
            create table events (
                id integer not null primary key,
                timestamp text not null,
                data json not null
            );
        ",
        )?;

        // people use maybe 100 different apps
        let app_names: Vec<String> = names::Generator::with_naming(names::Name::Plain)
            .take(100)
            .collect();
        // of maybe 10 different categories
        let app_types: Vec<String> = names::Generator::with_naming(names::Name::Plain)
            .take(10)
            .collect();
        use rand::distributions::WeightedIndex;
        use rand::prelude::*;

        let window_properties = &[
            (30, "_GTK_APPLICATION_ID"),
            (30, "_GTK_APPLICATION_OBJECT_PATH"),
            (30, "_GTK_UNIQUE_BUS_NAME"),
            (30, "_GTK_WINDOW_OBJECT_PATH"),
            (40, "_NET_WM_USER_TIME_WINDOW"),
            (41, "WM_CLIENT_LEADER"),
            (50, "_NET_WM_BYPASS_COMPOSITOR"),
            (60, "WM_WINDOW_ROLE"),
            (61, "_MOTIF_WM_HINTS"),
            (90, "_GTK_THEME_VARIANT"),
            (91, "_NET_WM_SYNC_REQUEST_COUNTER"),
            (91, "_NET_WM_USER_TIME"),
            (139, "_NET_STARTUP_ID"),
            (170, "_NET_WM_ICON_NAME"),
            (180, "WM_HINTS"),
            (220, "_NET_WM_WINDOW_TYPE"),
            (220, "XdndAware"),
            (229, "WM_LOCALE_NAME"),
            (230, "_NET_WM_NAME"),
            (230, "_NET_WM_PID"),
            (230, "WM_CLIENT_MACHINE"),
            (240, "_NET_WM_DESKTOP"),
            (240, "_NET_WM_STATE"),
            (240, "WM_CLASS"),
            (240, "WM_NORMAL_HINTS"),
            (240, "WM_PROTOCOLS"),
            (240, "WM_STATE"),
        ];

        let mut rng = thread_rng();
        let event_type_dist = WeightedIndex::new(&[10, 10, 1])?;
        let window_properties_dist = WeightedIndex::new(window_properties.iter().map(|e| e.0))?;
        let app_id_dist = rand::distributions::Uniform::from(0..100);
        let data = (1..100000).map(|_| match event_type_dist.sample(&mut rng) {
            0 => {
                let mut properties = BTreeMap::new();
                for i in 1..rand::distributions::Uniform::from(5..20).sample(&mut rng) {
                    let p = window_properties[window_properties_dist.sample(&mut rng)].1;
                    properties.insert(p.to_string(), "1".to_string());
                }
                EventData::OpenApplication {
                    id: app_id_dist.sample(&mut rng),
                    app_name: app_names.choose(&mut rng).unwrap().clone(),
                    app_type: app_types.choose(&mut rng).unwrap().clone(),
                    properties,
                }
            }
            1 => EventData::CloseApplication {
                id: app_id_dist.sample(&mut rng),
            },
            2 => EventData::Shutdown,
            _ => panic!("impossible"),
        });
        {
            let tx = db.transaction()?;
            let mut insert = tx.prepare("insert into events (timestamp, data) values (?, ?, ?)")?;
            for d in data {
                insert.execute(params![
                    chrono::Utc::now().to_rfc3339(),
                    serde_json::to_string_pretty(&d)?
                ])?;
            }
        }
        Ok(db)
    }

    #[test]
    fn sanity() -> anyhow::Result<()> {
        let db = create_example_db()?;

        Ok(())
    }
    //
    // check that zstd_enable_transparent only creates one dictionary on the full table UPDATE

    //
    // check that `insert into events values ('a', 'b', 'c', 'd', 'e', 'f'), ('b', 'c', 'd', 'e', 'f', 'g');`
    // only creates one dictionary

    // check that decompress only creates one dictionary
}
