//! Function-name inspection through the viewer's normal filtering and source tabs.
use std::path::Path;

use gpui::Context;

use super::ShellView;

impl ShellView {
    /// Reports the loaded full names and labels, and chooses local source tabs to visit.
    pub fn function_name_tour_frames(&self) -> anyhow::Result<Vec<usize>> {
        let Some(session) = &self.session else {
            return Ok(Vec::new());
        };
        let mut sampled_functions = std::collections::HashSet::new();
        if session.has_assembly {
            let recording = store::Session::open(&session.result_directory)?;
            let mut statement = recording.connection().prepare(
                "SELECT DISTINCT s.module_path, s.func_name
                     FROM assembly_samples s
                     JOIN assembly_lines l
                       ON l.module_path = s.module_path AND l.runtime_address = s.address
                     WHERE l.symbol IS NOT NULL",
            )?;
            sampled_functions = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<store::duckdb::Result<_>>()?;
        }
        let mut inspect = Vec::new();
        for frame in &session.profile.frames {
            if frame.name == frame.friendly_name {
                continue;
            }
            let local_source = frame
                .file
                .as_deref()
                .is_some_and(|file| Path::new(file).is_file());
            println!(
                "tour: function_name {}",
                serde_json::json!({
                    "id": frame.id, "full": frame.name, "friendly": frame.friendly_name,
                    "module": frame.module, "local_source": local_source,
                })
            );
            let sampled_function = frame.module.as_ref().is_some_and(|module| {
                sampled_functions.contains(&(module.clone(), frame.name.clone()))
            });
            if local_source && sampled_function {
                inspect.push(frame.id);
            }
        }
        Ok(inspect)
    }

    /// Applies both spellings to the normal symbol filter, then opens this exact frame.
    pub fn tour_function_name(
        &mut self,
        frame_id: usize,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let session = self
            .session
            .as_ref()
            .expect("tour requires a loaded session")
            .clone();
        let frame = &session.profile.frames[frame_id];
        for query in [&frame.name, &frame.friendly_name] {
            self.filter.symbol = query.to_owned();
            let resolved = self.filter.resolve(&session);
            anyhow::ensure!(
                resolved
                    .frame_ids
                    .as_ref()
                    .is_some_and(|frames| frames.contains(&frame_id)),
                "symbol search lost frame {frame_id}: {query}"
            );
        }
        self.filter.symbol.clear();
        self.refresh_analysis(cx);
        self.select_frame(Some(frame_id), cx);
        self.open_source_tab(frame_id, cx);
        Ok(())
    }

    /// Waits for the same source and assembly result that the source tab renders.
    pub fn function_name_tour_ready(&self, frame_id: usize) -> anyhow::Result<bool> {
        let session = self
            .session
            .as_ref()
            .expect("tour requires a loaded session");
        let tab = self
            .source_tabs
            .iter()
            .find(|tab| tab.frame_id == frame_id)
            .expect("tour opened source tab");
        let document = tab
            .document
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("source tab has no document"))?;
        anyhow::ensure!(
            document.error.is_none() && !document.lines.is_empty(),
            "source lookup failed: {:?}",
            document.error
        );
        if let Some(error) = &tab.asm_error {
            anyhow::bail!("assembly lookup failed: {error}");
        }
        if session.has_assembly && tab.asm.is_none() {
            return Ok(false);
        }
        if let Some(asm) = &tab.asm {
            anyhow::ensure!(
                !asm.instructions.is_empty(),
                "assembly lookup returned no instructions for {}",
                session.profile.frames[frame_id].name
            );
        }
        println!(
            "tour: function_inspection {}",
            serde_json::json!({
                "id": frame_id, "full": session.profile.frames[frame_id].name,
                "source": document.path, "instructions": tab.asm.as_ref().map(|asm| asm.instructions.len()),
                "full_search": true, "friendly_search": true,
            })
        );
        Ok(true)
    }
}
