use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    io::{self, BufWriter, Write},
    path::Path,
};

use anyhow::Result;

use crate::pprof::{Function, Location, Profile, Sample, atomic_write};

const SVG_WIDTH: f64 = 1200.0;
const HEADER_HEIGHT: f64 = 54.0;
const FRAME_HEIGHT: f64 = 18.0;
const MAX_SVG_NODES: usize = 100_000;

#[derive(Clone, Copy)]
pub enum FlameValue {
    Nanoseconds,
    Bytes,
}

#[derive(Default)]
struct FlameNode {
    value: u64,
    children: BTreeMap<String, FlameNode>,
}

pub fn write_flamegraph(
    path: &Path,
    profile: &Profile,
    value_index: usize,
    title: &str,
    unit: FlameValue,
) -> Result<()> {
    atomic_write(path, |file| {
        let mut output = BufWriter::new(file);
        render_flamegraph_to(&mut output, profile, value_index, title, unit)?;
        output.flush()?;
        Ok(())
    })
}

fn render_flamegraph_to(
    output: &mut impl Write,
    profile: &Profile,
    value_index: usize,
    title: &str,
    unit: FlameValue,
) -> io::Result<()> {
    let locations = profile
        .location
        .iter()
        .map(|location| (location.id, location))
        .collect::<HashMap<_, _>>();
    let functions = profile
        .function
        .iter()
        .map(|function| (function.id, function))
        .collect::<HashMap<_, _>>();
    let mut root = FlameNode::default();
    let mut node_count = 0;
    let mut truncated = false;
    for sample in &profile.sample {
        let Some(value) = sample
            .value
            .get(value_index)
            .copied()
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value != 0)
        else {
            continue;
        };
        let frames = sample_frames(profile, sample, &locations, &functions);
        insert_sample(&mut root, frames, value, &mut node_count, &mut truncated);
    }

    let depth = tree_depth(&root).max(1);
    let height = HEADER_HEIGHT + depth as f64 * FRAME_HEIGHT + 8.0;
    let escaped_title = escape_xml(title);
    let total = format_value(root.value, unit);
    let truncation = if truncated {
        format!("; visualization limited to {MAX_SVG_NODES} frames")
    } else {
        String::new()
    };
    write!(
        output,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {SVG_WIDTH:.0} {height:.0}\" width=\"{SVG_WIDTH:.0}\" height=\"{height:.0}\" role=\"img\" aria-labelledby=\"title description\">\n\
<title id=\"title\">{escaped_title}</title>\n\
<desc id=\"description\">Flame graph with total {total}{truncation}</desc>\n\
<style>text{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;fill:#172033}}.heading{{font:600 18px system-ui,sans-serif}}.summary{{font:12px system-ui,sans-serif;fill:#536078}}rect{{stroke:#fff;stroke-width:.5}}</style>\n\
<rect width=\"100%\" height=\"100%\" fill=\"#f7f8fb\"/>\n\
<text class=\"heading\" x=\"8\" y=\"22\">{escaped_title}</text>\n\
<text class=\"summary\" x=\"8\" y=\"42\">Total: {total}{truncation}</text>\n"
    )?;
    if root.value == 0 {
        output.write_all(
            b"<text class=\"summary\" x=\"8\" y=\"72\">No positive samples in this window</text>\n",
        )?;
    } else {
        render_children(output, &root, 0, depth, 0.0, SVG_WIDTH, root.value, unit)?;
    }
    output.write_all(b"</svg>\n")
}

fn sample_frames<'a>(
    profile: &'a Profile,
    sample: &Sample,
    locations: &HashMap<u64, &'a Location>,
    functions: &HashMap<u64, &'a Function>,
) -> Vec<Cow<'a, str>> {
    let mut frames = Vec::new();
    for location_id in sample.location_id.iter().rev() {
        let Some(location) = locations.get(location_id) else {
            continue;
        };
        if location.line.is_empty() {
            frames.push(Cow::Owned(format!("0x{:x}", location.address)));
            continue;
        }
        for line in location.line.iter().rev() {
            let label = functions
                .get(&line.function_id)
                .and_then(|function| usize::try_from(function.name).ok())
                .and_then(|name| profile.string_table.get(name))
                .filter(|name| !name.is_empty())
                .map(|name| Cow::Borrowed(name.as_str()))
                .unwrap_or_else(|| Cow::Owned(format!("0x{:x}", location.address)));
            frames.push(label);
        }
    }
    frames
}

fn insert_sample(
    root: &mut FlameNode,
    frames: Vec<Cow<'_, str>>,
    value: u64,
    node_count: &mut usize,
    truncated: &mut bool,
) {
    root.value = root.value.saturating_add(value);
    let mut node = root;
    for label in frames {
        if node.children.contains_key(label.as_ref()) {
            node = node
                .children
                .get_mut(label.as_ref())
                .expect("flame node exists");
        } else {
            if *node_count >= MAX_SVG_NODES {
                *truncated = true;
                break;
            }
            node = node.children.entry(label.into_owned()).or_default();
            *node_count += 1;
        }
        node.value = node.value.saturating_add(value);
    }
}

fn tree_depth(node: &FlameNode) -> usize {
    node.children
        .values()
        .map(|child| 1 + tree_depth(child))
        .max()
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn render_children(
    output: &mut impl Write,
    node: &FlameNode,
    depth: usize,
    max_depth: usize,
    x: f64,
    width: f64,
    total: u64,
    unit: FlameValue,
) -> io::Result<()> {
    if node.value == 0 {
        return Ok(());
    }
    let mut cursor = x;
    for (label, child) in &node.children {
        let child_width = width * child.value as f64 / node.value as f64;
        let child_x = cursor;
        cursor += child_width;
        if child_width <= 0.01 {
            continue;
        }
        let y = HEADER_HEIGHT + (max_depth - depth - 1) as f64 * FRAME_HEIGHT;
        let color = frame_color(label);
        let tooltip = escape_xml(&format!(
            "{} — {} ({:.2}%)",
            label,
            format_value(child.value, unit),
            child.value as f64 * 100.0 / total as f64
        ));
        write!(
            output,
            "<g><title>{tooltip}</title><rect x=\"{child_x:.2}\" y=\"{y:.2}\" width=\"{child_width:.2}\" height=\"{:.2}\" fill=\"{color}\"/>\n",
            FRAME_HEIGHT - 1.0
        )?;
        if let Some(label) = fitted_label(label, child_width) {
            writeln!(
                output,
                "<text x=\"{:.2}\" y=\"{:.2}\" font-size=\"11\">{}</text></g>",
                child_x + 3.0,
                y + 12.5,
                escape_xml(&label)
            )?;
        } else {
            output.write_all(b"</g>\n")?;
        }
        render_children(
            output,
            child,
            depth + 1,
            max_depth,
            child_x,
            child_width,
            total,
            unit,
        )?;
    }
    Ok(())
}

fn fitted_label(label: &str, width: f64) -> Option<String> {
    let capacity = (width / 7.0).floor() as usize;
    if capacity < 3 {
        return None;
    }
    let count = label.chars().count();
    if count <= capacity {
        return Some(label.to_owned());
    }
    let mut fitted = label
        .chars()
        .take(capacity.saturating_sub(1))
        .collect::<String>();
    fitted.push('…');
    Some(fitted)
}

fn frame_color(label: &str) -> String {
    let hash = label.bytes().fold(2_166_136_261_u32, |hash, byte| {
        hash.wrapping_mul(16_777_619) ^ u32::from(byte)
    });
    let red = 210 + hash % 35;
    let green = 80 + (hash >> 8) % 95;
    let blue = 55 + (hash >> 16) % 45;
    format!("#{red:02x}{green:02x}{blue:02x}")
}

fn format_value(value: u64, unit: FlameValue) -> String {
    match unit {
        FlameValue::Nanoseconds if value >= 1_000_000_000 => {
            format!("{:.2} s", value as f64 / 1_000_000_000.0)
        }
        FlameValue::Nanoseconds if value >= 1_000_000 => {
            format!("{:.2} ms", value as f64 / 1_000_000.0)
        }
        FlameValue::Nanoseconds if value >= 1_000 => {
            format!("{:.2} µs", value as f64 / 1_000.0)
        }
        FlameValue::Nanoseconds => format!("{value} ns"),
        FlameValue::Bytes if value >= 1024 * 1024 * 1024 => {
            format!("{:.2} GiB", value as f64 / (1024.0 * 1024.0 * 1024.0))
        }
        FlameValue::Bytes if value >= 1024 * 1024 => {
            format!("{:.2} MiB", value as f64 / (1024.0 * 1024.0))
        }
        FlameValue::Bytes if value >= 1024 => format!("{:.2} KiB", value as f64 / 1024.0),
        FlameValue::Bytes => format!("{value} B"),
    }
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character if character.is_control() && !matches!(character, '\t' | '\n' | '\r') => {
                escaped.push('�');
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{FlameValue, render_flamegraph_to};
    use crate::pprof::{Function, Line, Location, Profile, Sample, ValueType};

    #[test]
    fn render_flamegraph_escapes_untrusted_frame_labels() {
        let profile = Profile {
            sample_type: vec![ValueType { r#type: 1, unit: 2 }],
            sample: vec![Sample {
                location_id: vec![1],
                value: vec![7],
                label: Vec::new(),
            }],
            mapping: Vec::new(),
            location: vec![Location {
                id: 1,
                mapping_id: 0,
                address: 0x1000,
                line: vec![Line {
                    function_id: 1,
                    line: 1,
                    column: 0,
                }],
                is_folded: false,
            }],
            function: vec![Function {
                id: 1,
                name: 3,
                system_name: 0,
                filename: 0,
                start_line: 0,
            }],
            string_table: vec![
                String::new(),
                "samples".to_owned(),
                "nanoseconds".to_owned(),
                "<script>&\"'".to_owned(),
            ],
            drop_frames: 0,
            keep_frames: 0,
            time_nanos: 0,
            duration_nanos: 0,
            period_type: None,
            period: 0,
            comment: Vec::new(),
            default_sample_type: 0,
            doc_url: 0,
        };

        let mut output = Vec::new();
        render_flamegraph_to(
            &mut output,
            &profile,
            0,
            "CPU flame graph",
            FlameValue::Nanoseconds,
        )
        .expect("render SVG");
        let svg = String::from_utf8(output).expect("SVG is UTF-8");

        assert!(svg.contains("&lt;script&gt;&amp;&quot;&apos;"));
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("<rect"));
    }
}
