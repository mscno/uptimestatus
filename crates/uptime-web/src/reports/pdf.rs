//! Typesetting a [`Report`] as PDF with an embedded Typst: the template
//! (`report.typ`), the report as `/data.json`, the page's logo and the
//! bundled fonts (`fonts/`). Nothing is read from disk or the network.

use std::sync::LazyLock;

use jiff::{Timestamp, tz::TimeZone};
use typst::{
    Library, LibraryExt as _, World,
    diag::{FileError, FileResult, SourceDiagnostic},
    foundations::{Bytes, Datetime},
    syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot},
    text::{Font, FontBook},
    utils::LazyHash,
};
use typst_layout::PagedDocument;

use super::Report;

const TEMPLATE: &str = include_str!("report.typ");

const FONT_FILES: [&[u8]; 5] = [
    include_bytes!("fonts/IBMPlexSans-Regular.ttf"),
    include_bytes!("fonts/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("fonts/PixelifySans-SemiBold.ttf"),
];

/// Logos Typst can place, by file extension.
const IMAGE_FORMATS: [&str; 5] = ["png", "jpg", "gif", "webp", "svg"];

static LIBRARY: LazyLock<LazyHash<Library>> = LazyLock::new(|| LazyHash::new(Library::default()));

static FONTS: LazyLock<(LazyHash<FontBook>, Vec<Font>)> = LazyLock::new(|| {
    let fonts: Vec<Font> = FONT_FILES
        .iter()
        .flat_map(|data| Font::iter(Bytes::new(*data)))
        .collect();
    (LazyHash::new(FontBook::from_fonts(&fonts)), fonts)
});

/// Why a report could not be typeset.
#[derive(Debug, thiserror::Error)]
#[error("typesetting the report failed: {0}")]
pub(crate) struct RenderError(String);

/// A logo to show: its file extension and bytes.
pub(crate) struct Logo {
    pub(crate) extension: String,
    pub(crate) bytes: Vec<u8>,
}

impl Logo {
    /// The logo if Typst can show it (not ICO).
    pub(crate) fn supported(name: &str, bytes: Vec<u8>) -> Option<Self> {
        let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
        IMAGE_FORMATS
            .contains(&extension.as_str())
            .then_some(Self { extension, bytes })
    }
}

/// Typesets `report` as PDF (CPU-bound: call it off the async runtime).
pub(crate) fn render(report: &Report, logo: Option<&Logo>) -> Result<Vec<u8>, RenderError> {
    let document = layout(report, logo);
    let pdf = document.and_then(|document| {
        typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default())
            .map_err(|errors| RenderError(messages(&errors)))
    });
    // Typst memoizes across compilations; keep the cache from growing.
    typst::comemo::evict(1);
    pdf
}

/// Lays out the report's pages.
fn layout(report: &Report, logo: Option<&Logo>) -> Result<PagedDocument, RenderError> {
    let mut report = report.clone();
    report.logo = logo.map(|logo| format!("/logo.{}", logo.extension));
    let data = serde_json::to_vec(&report).map_err(|e| RenderError(e.to_string()))?;
    let world = ReportWorld::new(data, logo)?;
    typst::compile::<PagedDocument>(&world)
        .output
        .map_err(|errors| RenderError(located(&world, &errors)))
}

/// Error messages with the template line they come from.
fn located(world: &ReportWorld, errors: &[SourceDiagnostic]) -> String {
    errors
        .iter()
        .map(|error| {
            let line = typst::WorldExt::range(world, error.span)
                .and_then(|range| world.main.lines().byte_to_line(range.start));
            match line {
                Some(line) => format!("{} (report.typ line {})", error.message, line + 1),
                None => error.message.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn messages(errors: &[SourceDiagnostic]) -> String {
    errors
        .iter()
        .map(|error| error.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

fn file_id(path: &str) -> Result<FileId, RenderError> {
    let path = VirtualPath::new(path).map_err(|e| RenderError(format!("{e:?}")))?;
    Ok(RootedPath::new(VirtualRoot::Project, path).intern())
}

/// The template's world: one source, the report data and the logo.
struct ReportWorld {
    main: Source,
    data: (FileId, Bytes),
    logo: Option<(FileId, Bytes)>,
    today: Option<Datetime>,
}

impl ReportWorld {
    fn new(data: Vec<u8>, logo: Option<&Logo>) -> Result<Self, RenderError> {
        let main = Source::new(file_id("/report.typ")?, TEMPLATE.to_owned());
        let logo = match logo {
            Some(logo) => Some((
                file_id(&format!("/logo.{}", logo.extension))?,
                Bytes::new(logo.bytes.clone()),
            )),
            None => None,
        };
        let date = Timestamp::now().to_zoned(TimeZone::UTC).date();
        Ok(Self {
            main,
            data: (file_id("/data.json")?, Bytes::new(data)),
            logo,
            today: Datetime::from_ymd(
                i32::from(date.year()),
                u8::try_from(date.month()).unwrap_or(1),
                u8::try_from(date.day()).unwrap_or(1),
            ),
        })
    }
}

impl World for ReportWorld {
    fn library(&self) -> &LazyHash<Library> {
        &LIBRARY
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &FONTS.0
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else {
            Err(FileError::NotFound(id.vpath().get_without_slash().into()))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.data.0 {
            return Ok(self.data.1.clone());
        }
        match &self.logo {
            Some((logo, bytes)) if *logo == id => Ok(bytes.clone()),
            _ => Err(FileError::NotFound(id.vpath().get_without_slash().into())),
        }
    }

    fn font(&self, index: usize) -> Option<Font> {
        FONTS.1.get(index).cloned()
    }

    fn today(&self, _offset: Option<typst::foundations::Duration>) -> Option<Datetime> {
        self.today
    }
}

#[cfg(test)]
mod tests {
    use typst::layout::{Frame, FrameItem};

    use super::*;
    use crate::reports::{ComponentReport, IncidentReport, MaintenanceReport, SectionReport};

    fn sample(look: &str) -> Report {
        let component = |name: &str, uptime: &str, met| ComponentReport {
            name: name.into(),
            uptime: Some(uptime.into()),
            downtime: "43m 12s".into(),
            downtime_secs: 2592,
            maintenance: "2h".into(),
            met: Some(met),
            bars: [
                ["up"; 20].as_slice(),
                &["warn", "down", "empty"],
                &["future"; 7],
            ]
            .concat(),
        };
        Report {
            title: "PrivateNPM Status".into(),
            month: "2026-09".into(),
            label: "September 2026".into(),
            to_date: false,
            generated: "Oct 1, 2026, 00:05 UTC".into(),
            accent: "#c6f432".into(),
            look: look.into(),
            logo: None,
            target: Some("99.9%".into()),
            overall: Some("99.48%".into()),
            overall_met: Some(false),
            sections: vec![SectionReport {
                name: "Registries".into(),
                components: vec![
                    component("NPM Registry", "99.96%", true),
                    component("Pypi Repository", "99.00%", false),
                ],
            }],
            incidents: vec![IncidentReport {
                title: "NPM Registry is down".into(),
                impact: "Major".into(),
                started: "Sep 15, 02:33 UTC".into(),
                duration: "9m 50s".into(),
                components: vec!["NPM Registry".into()],
            }],
            maintenance: vec![MaintenanceReport {
                title: "Database upgrade".into(),
                starts: "Sep 20, 22:00 UTC".into(),
                ends: "Sep 21, 00:00 UTC".into(),
            }],
            excused: 3,
            methodology: vec!["Uptime is the share of checks that were not down.".into()],
        }
    }

    /// The text laid out on every page, in order.
    fn text(document: &PagedDocument) -> String {
        fn walk(frame: &Frame, out: &mut String) {
            for (_, item) in frame.items() {
                match item {
                    FrameItem::Group(group) => walk(&group.frame, out),
                    FrameItem::Text(text) => {
                        out.push_str(&text.text);
                        out.push(' ');
                    }
                    _ => {}
                }
            }
        }
        let mut out = String::new();
        for page in document.pages() {
            walk(&page.frame, &mut out);
        }
        out
    }

    #[test]
    fn reports_typeset_their_figures() {
        for look in ["pixel", "clean"] {
            let document = layout(&sample(look), None).unwrap();
            let text = text(&document);
            for expected in [
                "PrivateNPM Status",
                "September 2026",
                "99.48%",
                "99.9%",
                "Missed",
                "NPM Registry",
                "99.96%",
                "Pypi Repository",
                "43m 12s",
                "NPM Registry is down",
                "9m 50s",
                "Database upgrade",
                "3 failed checks were excluded",
                "How uptime is measured",
            ] {
                assert!(
                    text.contains(expected),
                    "{look}: {expected:?} missing from {text}"
                );
            }
        }
    }

    #[test]
    fn reports_render_as_pdf_with_a_logo() {
        // A 1x1 PNG.
        let png = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52,
            0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d, 0x49,
            0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0, 0x05, 0, 0x01,
            0xff, 0x89, 0x99, 0x3d, 0x1d, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
            0x82,
        ];
        let logo = Logo::supported("logo.png", png).unwrap();
        let pdf = render(&sample("pixel"), Some(&logo)).unwrap();
        assert!(pdf.starts_with(b"%PDF-"), "a PDF");
        assert!(pdf.len() > 5_000);
        assert!(Logo::supported("favicon.ico", Vec::new()).is_none());
    }

    /// `REPORT_SAMPLE=/tmp/x.pdf cargo test -p uptime-web --lib sample_report -- --ignored`
    /// writes a sample report, for checking the design.
    #[test]
    #[ignore = "writes a file for people to look at"]
    fn sample_report() {
        let path = std::env::var("REPORT_SAMPLE").unwrap();
        std::fs::write(path, render(&sample("pixel"), None).unwrap()).unwrap();
    }

    /// Times a large report: `cargo test --release -p uptime-web --lib
    /// render_cost -- --ignored --nocapture` (measure memory around it).
    #[test]
    #[ignore = "a measurement, not a check"]
    fn render_cost() {
        let mut report = sample("pixel");
        let one = report.sections[0].components[0].clone();
        report.sections = (0..3)
            .map(|s| SectionReport {
                name: format!("Section {s}"),
                components: (0..4)
                    .map(|c| ComponentReport {
                        name: format!("Component {s}.{c}"),
                        ..one.clone()
                    })
                    .collect(),
            })
            .collect();
        report.incidents = (0..10).map(|_| report.incidents[0].clone()).collect();
        for round in 0..5 {
            let started = std::time::Instant::now();
            let pdf = render(&report, None).unwrap();
            eprintln!(
                "render {round}: {:?}, {} bytes",
                started.elapsed(),
                pdf.len()
            );
        }
    }

    #[test]
    fn empty_months_typeset_too() {
        let report = Report {
            sections: Vec::new(),
            incidents: Vec::new(),
            maintenance: Vec::new(),
            overall: None,
            overall_met: None,
            target: None,
            excused: 0,
            to_date: true,
            ..sample("clean")
        };
        let text = text(&layout(&report, None).unwrap());
        assert!(text.contains("month to date"), "{text}");
        assert!(text.contains("No incidents this month."), "{text}");
    }
}
