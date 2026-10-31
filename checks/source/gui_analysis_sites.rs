pub(super) fn values(rows: &[(u64, Site)]) -> Vec<serde_json::Value> {
    let mut rows = rows.iter().collect::<Vec<_>>();
    rows.sort_by_key(|row| row.0);
    rows.into_iter()
        .map(|(stack, site)| {
            serde_json::json!({
                "stack": stack,
                "title": site.title,
                "count": site.count,
                "requested": site.requested,
                "live": site.live,
                "ended_lifetime": site.ended_lifetime,
                "ended_count": site.ended_count,
                "peak": site.peak,
            })
        })
        .collect()
}

// The independent builder input is resolved by the check, not by the job.
pub(super) fn expected_allocation_sites(
    session: &ShellSession,
    filter: &SampleFilter,
) -> Vec<(u64, Site)> {
    allocation_sites(session, filter)
}
