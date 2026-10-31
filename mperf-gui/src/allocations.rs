use crate::{
    profile::{ProfileData, ProfileFrame},
    sql::{Connection, table_columns},
};
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct AllocationData {
    pub error: Option<String>,
    pub complete: bool,
    pub count: u64,
    pub bytes: u64,
    pub live: u64,
    pub peak: u64,
    pub attributed: u64,
    pub lifetimes: Vec<Lifetime>,
    pub stacks: HashMap<u64, Vec<usize>>,
    pub timeline: Vec<(u64, f64)>,
}
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Boundary {
    pub time: u64,
    pub sequence: u64,
}
pub(crate) struct Lifetime {
    pub pid: u32,
    pub stack: u64,
    pub start: Boundary,
    pub end: Option<Boundary>,
    pub bytes: u64,
    pub tid: u32,
}
impl AllocationData {
    pub fn load(connection: &Connection, profile: &mut ProfileData) -> Option<Self> {
        if table_columns(connection, "allocation_summary").is_empty() {
            return None;
        }
        Some(
            Self::load_inner(connection, profile).unwrap_or_else(|error| Self {
                error: Some(format!("{error:#}")),
                ..Self::default()
            }),
        )
    }
    fn load_inner(connection: &Connection, profile: &mut ProfileData) -> anyhow::Result<Self> {
        let mut frames = Vec::new();
        let mut data = connection.query_row(
            "SELECT complete,
                    observed_count,
                    requested_bytes::UBIGINT,
                    live_at_end_bytes::UBIGINT,
                    peak_observed_bytes,
                    attributed_count
             FROM allocation_summary",
            [],
            |r| {
                Ok(Self {
                    error: None,
                    complete: r.get::<_, i64>(0)? == 1,
                    count: r.get(1)?,
                    bytes: r.get(2)?,
                    live: r.get(3)?,
                    peak: r.get(4)?,
                    attributed: r.get(5)?,
                    lifetimes: Vec::new(),
                    stacks: HashMap::new(),
                    timeline: Vec::new(),
                })
            },
        )?;
        let mut statement = connection.prepare(
            "SELECT stack_id, allocated_ns, freed_ns, requested_bytes, tid, pid,
                    allocation_id, freed_sequence
             FROM allocations
             ORDER BY allocated_ns, allocation_id",
        )?;
        data.lifetimes = statement
            .query_map([], |r| {
                Ok(Lifetime {
                    pid: r.get::<_, u64>(5)? as u32,
                    stack: r.get(0)?,
                    start: Boundary {
                        time: r.get(1)?,
                        sequence: r.get(6)?,
                    },
                    end: match r.get::<_, Option<i64>>(2)? {
                        Some(time) => Some(Boundary {
                            time: time as u64,
                            sequence: r.get::<_, i64>(7)? as u64,
                        }),
                        None => None,
                    },
                    bytes: r.get(3)?,
                    tid: r.get::<_, u64>(4)? as u32,
                })
            })?
            .collect::<Result<_, _>>()?;
        let mut statement = connection.prepare(
            "SELECT stack_id, function, file, line, module
             FROM allocation_stacks
             ORDER BY stack_id, frame_index, inline_index",
        )?;
        let rows = statement.query_map([], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (stack, name, file, line, module) = row?;
            let id = profile.frames.len() + frames.len();
            frames.push(ProfileFrame {
                id,
                name,
                module,
                file: (!file.is_empty()).then_some(file),
                line: (line > 0).then_some(line as u32),
            });
            data.stacks.entry(stack).or_default().push(id);
        }
        let mut statement = connection.prepare(
            "WITH buckets AS (
                 SELECT timestamp_ns,
                        observed_live_bytes,
                        NTILE(512) OVER(ORDER BY timestamp_ns) AS bucket
                 FROM allocation_timeline
             ), points AS (
                 SELECT MIN(timestamp_ns) AS timestamp_ns,
                        MAX(observed_live_bytes)::DOUBLE AS bytes
                 FROM buckets
                 GROUP BY bucket
                 UNION ALL
                 SELECT timestamp_ns, observed_live_bytes::DOUBLE
                 FROM (
                     SELECT *
                     FROM allocation_timeline
                     ORDER BY timestamp_ns DESC
                     LIMIT 1
                 )
             )
             SELECT timestamp_ns, bytes
             FROM points
             ORDER BY timestamp_ns",
        )?;
        data.timeline = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        profile.frames.extend(frames);
        Ok(data)
    }
}
