use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use ratatui::{
    layout::Constraint,
    style::{Style, Stylize},
    widgets::{Block, Cell, Paragraph, Row, Table, Widget, Wrap},
};
use store::Session;

#[derive(Clone)]
pub struct LoopsTab {
    hotspots: Arc<RwLock<Vec<Loop>>>,
    is_running: Arc<RwLock<bool>>,
    session: Arc<Mutex<Session>>,
    load_error: Arc<RwLock<Option<String>>>,
}

#[allow(dead_code)]
struct Loop {
    function_name: String,
    file_name: String,
    line: u32,
    sint_ops: f64,
    sint_ai: f64,
    sfp_ops: f64,
    sfp_ai: f64,
    sdp_ops: f64,
    sdp_ai: f64,
    vint_ops: f64,
    vint_ai: f64,
    vfp_ops: f64,
    vfp_ai: f64,
    vdp_ops: f64,
    vdp_ai: f64,
}

impl Widget for LoopsTab {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer)
    where
        Self: Sized,
    {
        if let Some(error) = self.load_error.read().clone() {
            Paragraph::new(error)
                .block(Block::bordered().title("Roofline error"))
                .wrap(Wrap { trim: true })
                .render(area, buf);
            return;
        }

        let hotspots = self.hotspots.read();

        if hotspots.is_empty() {
            return;
        }

        let header = [
            Cell::from("Function"),
            Cell::from("Location"),
            Cell::from("Scalar SP GFLOP/s"),
            Cell::from("Scalar SP AI"),
            Cell::from("Scalar DP GFLOP/s"),
            Cell::from("Scalar DP AI"),
            Cell::from("Vector SP GFLOP/s"),
            Cell::from("Vector SP AI"),
            Cell::from("Vector DP GFLOP/s"),
            Cell::from("Vector DP AI"),
        ]
        .into_iter()
        .collect::<Row>()
        .style(Style::new().bold())
        .height(2);

        let rows = hotspots.iter().map(|loop_| {
            [
                Cell::from(
                    symbolize::FunctionName::new(&loop_.function_name)
                        .friendly()
                        .to_owned(),
                ),
                Cell::from(format!("{}:{}", loop_.file_name, loop_.line)),
                Cell::from(format!("{:.2}", loop_.sfp_ops)),
                Cell::from(format!("{:.2}", loop_.sfp_ai)),
                Cell::from(format!("{:.2}", loop_.sdp_ops)),
                Cell::from(format!("{:.2}", loop_.sdp_ai)),
                Cell::from(format!("{:.2}", loop_.vfp_ops)),
                Cell::from(format!("{:.2}", loop_.vfp_ai)),
                Cell::from(format!("{:.2}", loop_.vdp_ops)),
                Cell::from(format!("{:.2}", loop_.vdp_ai)),
            ]
            .into_iter()
            .collect::<Row>()
        });

        let widths = [
            Constraint::Max(30),
            Constraint::Min(40),
            Constraint::Max(20),
            Constraint::Max(20),
            Constraint::Max(20),
            Constraint::Max(20),
            Constraint::Max(20),
            Constraint::Max(20),
            Constraint::Max(20),
            Constraint::Max(20),
        ];

        let t = Table::new(rows, widths)
            .header(header)
            .block(Block::bordered());

        t.render(area, buf);
    }
}

impl LoopsTab {
    pub fn new(session: Arc<Mutex<Session>>) -> Self {
        LoopsTab {
            hotspots: Arc::new(RwLock::new(Vec::new())),
            is_running: Arc::new(RwLock::new(false)),
            session,
            load_error: Arc::new(RwLock::new(None)),
        }
    }

    pub fn run(&self) {
        {
            let hotspots = self.hotspots.read();
            if !hotspots.is_empty() || *self.is_running.read() {
                return;
            }
        }
        *self.is_running.write() = true;
        let this = self.clone();
        tokio::spawn(this.fetch_data());
    }

    async fn fetch_data(self) {
        let session = self.session.lock();
        let result: Result<Vec<Loop>, String> = (|| {
            if !session.has_table("roofline") {
                return Err("this recording has no roofline table".to_string());
            }
            let mut statement = session
                .connection()
                .prepare("SELECT * FROM roofline")
                .map_err(|error| error.to_string())?;
            let mut rows = statement.query([]).map_err(|error| error.to_string())?;

            let mut loops = Vec::new();
            while let Some(row) = rows.next().map_err(|error| error.to_string())? {
                let float = |column| {
                    row.get::<_, Option<f64>>(column)
                        .map(|value| value.unwrap_or_default())
                        .map_err(|error| error.to_string())
                };
                loops.push(Loop {
                    function_name: row
                        .get::<_, String>("function_name")
                        .map_err(|error| error.to_string())?,
                    file_name: row
                        .get::<_, String>("file_name")
                        .map_err(|error| error.to_string())?,
                    line: row
                        .get::<_, i64>("line")
                        .map_err(|error| error.to_string())? as u32,
                    sint_ops: float("scalar_int_ops")? / 1_000_000_000.0,
                    sint_ai: float("scalar_int_ai")?,
                    sfp_ops: float("scalar_float_ops")? / 1_000_000_000.0,
                    sfp_ai: float("scalar_float_ai")?,
                    sdp_ops: float("scalar_double_ops")? / 1_000_000_000.0,
                    sdp_ai: float("scalar_double_ai")?,
                    vint_ops: float("vector_int_ops")? / 1_000_000_000.0,
                    vint_ai: float("vector_int_ai")?,
                    vfp_ops: float("vector_float_ops")? / 1_000_000_000.0,
                    vfp_ai: float("vector_float_ai")?,
                    vdp_ops: float("vector_double_ops")? / 1_000_000_000.0,
                    vdp_ai: float("vector_double_ai")?,
                });
            }
            Ok(loops)
        })();
        drop(session);

        let rows = match result {
            Ok(rows) => rows,
            Err(error) => {
                *self.load_error.write() =
                    Some(format!("Could not load roofline data:\n\n{error}"));
                return;
            }
        };

        let mut hotspots = self.hotspots.write();
        *hotspots = rows;
    }
}
