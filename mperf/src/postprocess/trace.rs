use anyhow::Result;

use store::Session;

/// Materialize the defined default view over user/runtime trace events: one
/// row per trace point and kind, grouped by name/function — what renders as
/// a table in the hotspots view when no visualization manifest is attached.
pub(crate) fn write_custom_events(tables: &Session) -> Result<()> {
    if !tables.has_table("events") {
        return Ok(());
    }
    let payload_name = if tables.has_table("payloads") {
        "COALESCE(ns.string, '')"
    } else {
        "''"
    };
    let joins = if tables.has_table("payloads") {
        "LEFT JOIN payloads p ON p.event_id = source.event_id
         LEFT JOIN strings ns ON ns.id = p.name_id
         LEFT JOIN strings fs ON fs.id = p.function_id
         LEFT JOIN strings fls ON fls.id = p.file_id"
    } else {
        ""
    };
    let function = if tables.has_table("payloads") {
        "COALESCE(fs.string, '')"
    } else {
        "''"
    };
    let file = if tables.has_table("payloads") {
        "COALESCE(fls.string, '')"
    } else {
        "''"
    };
    let line = if tables.has_table("payloads") {
        "COALESCE(p.line, 0)"
    } else {
        "0"
    };
    tables.write_query(
        "custom_events",
        &format!(
            "WITH spans AS (
                 SELECT b.event_id, e.timestamp - b.timestamp AS duration_ns
                 FROM (SELECT instance, event_id, timestamp FROM events WHERE \"type\" = 0) b
                 JOIN (SELECT flow_id, timestamp FROM events WHERE \"type\" = 1 AND flow_id <> 0) e
                   ON e.flow_id = b.instance
             ),
             source AS (
                 SELECT event_id, 'span' AS kind, COUNT(*)::BIGINT AS count,
                        SUM(duration_ns)::BIGINT AS total_ns, 0::BIGINT AS total_value
                 FROM spans GROUP BY event_id
                 UNION ALL
                 SELECT event_id, 'instant', COUNT(*)::BIGINT, 0::BIGINT,
                        SUM(value)::BIGINT
                 FROM events WHERE \"type\" = 2 GROUP BY event_id
                 UNION ALL
                 SELECT event_id, 'counter', COUNT(*)::BIGINT, 0::BIGINT,
                        SUM(value)::BIGINT
                 FROM events WHERE \"type\" = 3 GROUP BY event_id
                 UNION ALL
                 SELECT event_id, 'loss', COUNT(*)::BIGINT, 0::BIGINT,
                        SUM(value)::BIGINT
                 FROM events WHERE \"type\" = 4 GROUP BY event_id
             )
             SELECT {payload_name} AS name, {function} AS function, {file} AS file,
                    {line} AS line, source.kind, source.count, source.total_ns,
                    source.total_value
             FROM source
             {joins}
             ORDER BY source.total_ns DESC, source.count DESC"
        ),
    )
}
