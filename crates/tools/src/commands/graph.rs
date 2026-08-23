// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::Display;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::bail;
use clap::builder::PossibleValue;
use clap::{Args, ValueEnum};
use cli::Output;
use dot_generator::{attr, edge, id, node, node_id};
use dot_structures::{
    Attribute,
    Edge,
    EdgeTy,
    Graph,
    GraphAttributes,
    Id,
    Node,
    NodeId,
    Stmt,
    Vertex,
};
use graphviz_rust::printer::{DotPrinter, PrinterContext};
use graphviz_rust::{cmd, exec};
use interpreter::InterpreterConstraint;
use pex::{Pex, PexPath};
use python_platform::PythonPlatform;

use crate::resolve::resolve;

#[derive(Clone)]
struct Format(cmd::Format);

impl Format {
    fn as_command_arg(&self) -> cmd::CommandArg {
        self.0.into()
    }

    const fn as_str(&self) -> &'static str {
        match self.0 {
            cmd::Format::Bmp => "bmp",
            cmd::Format::Cgimage => "cgimage",
            cmd::Format::Canon => "cannon",
            cmd::Format::Dot => "dot",
            cmd::Format::Gv => "gv",
            cmd::Format::Xdot => "xdot",
            cmd::Format::Xdot12 => "xdot1.2",
            cmd::Format::Xdot14 => "xdot1.4",
            cmd::Format::Eps => "eps",
            cmd::Format::Exr => "exr",
            cmd::Format::Fig => "fig",
            cmd::Format::Gd => "gd",
            cmd::Format::Gd2 => "gd2",
            cmd::Format::Gif => "gif",
            cmd::Format::Gtk => "gtk",
            cmd::Format::Ico => "ico",
            cmd::Format::Cmap => "cmap",
            cmd::Format::Ismap => "ismap",
            cmd::Format::Imap => "imap",
            cmd::Format::Cmapx => "cmapx",
            cmd::Format::ImapNp => "imap_np",
            cmd::Format::CmapxNp => "cmapx_np",
            cmd::Format::Jpg => "jpg",
            cmd::Format::Jpeg => "jpeg",
            cmd::Format::Jpe => "jpe",
            cmd::Format::Jp2 => "jp2",
            cmd::Format::Json => "json",
            cmd::Format::Json0 => "json0",
            cmd::Format::DotJson => "dot_json",
            cmd::Format::XdotJson => "xdot_json",
            cmd::Format::Pdf => "pdf",
            cmd::Format::Pic => "pic",
            cmd::Format::Pct => "pct",
            cmd::Format::Pict => "pict",
            cmd::Format::Plain => "plain",
            cmd::Format::PlainExt => "plain-ext",
            cmd::Format::Png => "png",
            cmd::Format::Pov => "pov",
            cmd::Format::Ps => "ps",
            cmd::Format::Ps2 => "ps2",
            cmd::Format::Psd => "psd",
            cmd::Format::Sgi => "sgi",
            cmd::Format::Svg => "svg",
            cmd::Format::Svgz => "svgz",
            cmd::Format::Tga => "tga",
            cmd::Format::Tif => "tif",
            cmd::Format::Tiff => "tiff",
            cmd::Format::Tk => "tk",
            cmd::Format::Vml => "vml",
            cmd::Format::Vmlz => "vmlz",
            cmd::Format::Vrml => "vrml",
            cmd::Format::Vbmp => "vbmp",
            cmd::Format::Webp => "webp",
            cmd::Format::Xlib => "xlib",
            cmd::Format::X11 => "x11",
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.write_str(self.as_str())
    }
}

impl ValueEnum for Format {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            Self(cmd::Format::Bmp),
            Self(cmd::Format::Cgimage),
            Self(cmd::Format::Canon),
            Self(cmd::Format::Dot),
            Self(cmd::Format::Gv),
            Self(cmd::Format::Xdot),
            Self(cmd::Format::Xdot12),
            Self(cmd::Format::Xdot14),
            Self(cmd::Format::Eps),
            Self(cmd::Format::Exr),
            Self(cmd::Format::Fig),
            Self(cmd::Format::Gd),
            Self(cmd::Format::Gd2),
            Self(cmd::Format::Gif),
            Self(cmd::Format::Gtk),
            Self(cmd::Format::Ico),
            Self(cmd::Format::Cmap),
            Self(cmd::Format::Ismap),
            Self(cmd::Format::Imap),
            Self(cmd::Format::Cmapx),
            Self(cmd::Format::ImapNp),
            Self(cmd::Format::CmapxNp),
            Self(cmd::Format::Jpg),
            Self(cmd::Format::Jpeg),
            Self(cmd::Format::Jpe),
            Self(cmd::Format::Jp2),
            Self(cmd::Format::Json),
            Self(cmd::Format::Json0),
            Self(cmd::Format::DotJson),
            Self(cmd::Format::XdotJson),
            Self(cmd::Format::Pdf),
            Self(cmd::Format::Pic),
            Self(cmd::Format::Pct),
            Self(cmd::Format::Pict),
            Self(cmd::Format::Plain),
            Self(cmd::Format::PlainExt),
            Self(cmd::Format::Png),
            Self(cmd::Format::Pov),
            Self(cmd::Format::Ps),
            Self(cmd::Format::Ps2),
            Self(cmd::Format::Psd),
            Self(cmd::Format::Sgi),
            Self(cmd::Format::Svg),
            Self(cmd::Format::Svgz),
            Self(cmd::Format::Tga),
            Self(cmd::Format::Tif),
            Self(cmd::Format::Tiff),
            Self(cmd::Format::Tk),
            Self(cmd::Format::Vml),
            Self(cmd::Format::Vmlz),
            Self(cmd::Format::Vrml),
            Self(cmd::Format::Vbmp),
            Self(cmd::Format::Webp),
            Self(cmd::Format::Xlib),
            Self(cmd::Format::X11),
        ]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.as_str()))
    }
}

#[derive(Args)]
pub(crate) struct GraphArgs {
    #[command(flatten)]
    output: Output,

    /// Attempt to render the graph.
    #[arg(short = 'r', long, default_value_t = false)]
    render: bool,

    /// The format to render the graph in.
    #[arg(short = 'f', long, default_value_t = Format(cmd::Format::Svg))]
    format: Format,

    /// Attempt to open the graph in the system viewer (implies --render).
    #[arg(long, default_value_t = false)]
    open: bool,
}

pub(crate) fn create(python: Option<&Path>, pex: Pex, args: GraphArgs) -> anyhow::Result<()> {
    args.output.configure()?;

    let pex_path = PexPath::from_pex_info(pex.info.raw(), true);
    let additional_pexes = pex_path.load_pexes()?;
    let (interpreter, wheels) = resolve(python, &pex, &additional_pexes)?;

    let mut graph = Graph::DiGraph {
        id: id!(esc pex.path.display()),
        strict: true,
        stmts: Vec::new(),
    };
    let graph_label = format!(
        "Dependency graph of {pex} for interpreter {python_binary} ({python_id})",
        pex = pex.path.display(),
        python_binary = interpreter.details.path.display(),
        python_id = InterpreterConstraint::exact_version(&interpreter)
    );
    graph.add_stmt(Stmt::GAttribute(GraphAttributes::Graph(vec![
        attr!("fontsize", 14),
        attr!("labelloc", esc "t"),
        attr!("label", esc graph_label),
    ])));

    for (project_name, wheel_info) in &wheels {
        let node_label = format!(
            "{name} {version}",
            name = wheel_info.raw_project_name,
            version = wheel_info.raw_version
        );
        let url = format!(
            "https://pypi.org/project/{name}/{version}",
            name = wheel_info.raw_project_name,
            version = wheel_info.raw_version
        );
        graph.add_stmt(Stmt::Node(node!(
            esc project_name;
            attr!("label", esc node_label),
            attr!("URL", esc url),
            attr!("target", esc "_blank")
        )));
        for requirement in &wheel_info.requires_dists {
            if !wheels.contains_key(&requirement.name)
                && !requirement.marker.evaluate(interpreter.marker_env(), &[])
            {
                let url = format!("https://pypi.org/project/{name}", name = requirement.name);
                graph.add_stmt(Stmt::Node(node!(
                    esc requirement.name;
                    attr!("color", esc "lightgrey"),
                    attr!("style", esc "filled"),
                    attr!("tooltip", esc "inactive requirement"),
                    attr!("URL", esc url),
                    attr!("target", esc "_blank")
                )))
            }
            let mut edge = edge!(
                node_id!(esc project_name) => node_id!(esc requirement.name);
                attr!("fontsize", 10)
            );
            if let Some(label) = match (
                &requirement.version_or_url,
                &requirement.marker.try_to_string(),
            ) {
                (Some(version_or_url), Some(marker)) => Some(format!("{version_or_url}; {marker}")),
                (Some(version_or_url), None) => Some(format!("{version_or_url}")),
                (None, Some(marker)) => Some(format!("; {marker}")),
                _ => None,
            } {
                edge.attributes.push(attr!("label", esc label))
            }
            graph.add_stmt(Stmt::Edge(edge))
        }
    }

    let mut ctx = PrinterContext::default();
    if args.render || args.open {
        let fmt = args.format;
        let rendered = match exec(graph, &mut ctx, vec![fmt.as_command_arg()]) {
            Ok(data) => data,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                bail!(
                    "Do you have `dot` installed on the $PATH?: {err}\n\
                    Find more information on `dot` at https://graphviz.org/."
                )
            }
            Err(err) => bail!(
                "Failed to render dependency graph for {pex}: {err}",
                pex = pex.path.display()
            ),
        };
        if args.open {
            let mut file = args
                .output
                .file(Some("pexrc-tools-graph."), Some(&format!(".deps.{fmt}")))?;
            file.write_all(rendered.as_slice())?;
            open::that_detached(file.path())?;
        } else {
            let mut output = args.output.writer()?;
            output.write_all(rendered.as_slice())?;
        }
    } else {
        let mut output = args.output.writer()?;
        writeln!(&mut output, "{graph}", graph = graph.print(&mut ctx))?;
    }
    Ok(())
}
