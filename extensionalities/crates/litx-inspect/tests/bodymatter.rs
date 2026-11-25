//! Integration tests for `glassbox_inspect::bodymatter` (`V-IO-05`).
//!
//! Verifies deterministic `bodymatter:v1` marker serialization, lossless XML
//! round-trip decoding via `quick_xml::Reader`, `--` absence inside HTML
//! comment payloads, anti-forgery `<!--` neutralization in body text and table
//! cells, minimal line-leading Markdown escaping, and GFM table formatting.

use glassbox_inspect::bodymatter::{
    BODYMATTER_PREFIX, BodymatterMarker, escape_attr, escape_table_cell, format_marker,
    render_gfm_table, sanitize_body,
};
use glassbox_inspect::reports::ContentRow;
use quick_xml::{Reader, XmlVersion, events::Event};

/// Reports whether `ch` is a legal XML 1.0 §2.2 character.
///
/// Accepts any Unicode scalar value. Returns `true` when `ch` satisfies the
/// XML 1.0 `Char` production (`#x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD]
/// | [#x10000-#x10FFFF]`), else `false`. Cannot fail.
fn is_xml_1_0_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000D}'
            | '\u{0020}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

/// Reports whether `ch` is a line terminator split by ASCII or Unicode `splitlines()`.
///
/// Accepts any Unicode scalar value. Returns `true` for LF, CR, VT, FF, FS, GS,
/// RS, NEL (`U+0085`), LS (`U+2028`), and PS (`U+2029`), else `false`. Cannot fail.
fn is_line_terminator(ch: char) -> bool {
    matches!(
        ch,
        '\n' | '\r'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{001C}'
            | '\u{001D}'
            | '\u{001E}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// Matches one line against the public consumer grammar `^<!-- bodymatter:v1 (<.*/>) -->$`.
///
/// Accepts a single line without line terminators. Returns `Some(xml_element)`
/// when the line matches the exact `bodymatter:v1` marker envelope, or `None`
/// otherwise. Cannot fail.
fn extract_marker_xml(line: &str) -> Option<&str> {
    if line.chars().any(is_line_terminator) {
        return None;
    }
    let inner = line
        .strip_prefix("<!-- ")?
        .strip_prefix(BODYMATTER_PREFIX)?
        .strip_prefix(' ')?
        .strip_suffix(" -->")?;
    if inner.starts_with('<') && inner.ends_with("/>") {
        Some(inner)
    } else {
        None
    }
}

/// Parses one `bodymatter:v1` marker line with `quick_xml::Reader` and checks comment invariants.
///
/// Accepts a formatted marker line. Returns `(element_name, attributes)` where
/// each attribute value is decoded with XML 1.0 attribute normalization. Panics
/// if the line breaks single-line framing, contains XML 1.0 §2.2 forbidden
/// scalars, contains `--` inside the comment payload, or fails strict XML parsing.
fn parse_marker_line(line: &str) -> (String, Vec<(String, String)>) {
    let comment_body = line
        .strip_prefix("<!-- ")
        .and_then(|rest| rest.strip_suffix(" -->"))
        .unwrap_or_else(|| panic!("line is not a single-line HTML comment: {line:?}"));
    assert!(
        !comment_body.contains("--"),
        "HTML comment payload must never contain `--`: {line:?}"
    );
    assert!(
        !line.chars().any(is_line_terminator),
        "marker must be strictly single-line with zero raw line terminators: {line:?}"
    );
    assert!(
        line.chars().all(is_xml_1_0_char),
        "marker must contain only XML 1.0 legal scalars: {line:?}"
    );

    let xml = extract_marker_xml(line)
        .unwrap_or_else(|| panic!("line did not match consumer marker pattern: {line:?}"));

    let mut reader = Reader::from_str(xml);
    let event = reader
        .read_event()
        .unwrap_or_else(|error| panic!("quick_xml failed to parse {xml:?}: {error}"));

    let (tag, attrs) = match event {
        Event::Empty(element) => {
            let tag = element.name().as_ref().to_owned();
            let mut attrs = Vec::new();
            for attr in element.attributes() {
                let attr = attr
                    .unwrap_or_else(|error| panic!("invalid XML attribute in {xml:?}: {error}"));
                let key = attr.key.as_ref().to_owned();
                let value = attr
                    .normalized_value(XmlVersion::default())
                    .unwrap_or_else(|error| {
                        panic!("failed to decode XML attribute `{key}` in {xml:?}: {error}")
                    })
                    .into_owned();
                attrs.push((key, value));
            }
            (tag, attrs)
        }
        other => panic!("expected single self-closing XML element in {xml:?}, got {other:?}"),
    };

    let trailing = reader
        .read_event()
        .unwrap_or_else(|error| panic!("quick_xml failed after element in {xml:?}: {error}"));
    assert!(
        matches!(trailing, Event::Eof),
        "expected EOF after marker element in {xml:?}, got {trailing:?}"
    );

    (tag, attrs)
}

/// Counts unescaped GFM pipe delimiters in one rendered table line.
///
/// Accepts a single table line. Returns the number of `|` characters preceded
/// by an even number of consecutive backslashes. Cannot fail.
fn count_unescaped_pipes(line: &str) -> usize {
    let mut backslashes = 0usize;
    let mut pipes = 0usize;
    for ch in line.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else {
            if ch == '|' && backslashes.is_multiple_of(2) {
                pipes += 1;
            }
            backslashes = 0;
        }
    }
    pipes
}

/// Test expectation for one serialized `bodymatter:v1` marker variant.
struct MarkerCase<'a> {
    marker: BodymatterMarker<'a>,
    expected_line: &'a str,
    expected_tag: &'a str,
    expected_attrs: &'a [(&'a str, &'a str)],
}

/// Verifies every `BodymatterMarker` variant's exact serialization, attribute order, and XML parsing.
#[test]
fn all_marker_variants_emit_deterministic_xml_and_round_trip() {
    let cases = [
        MarkerCase {
            marker: BodymatterMarker::Page {
                number: 1,
                label: None,
                origin: "native",
                partial: false,
            },
            expected_line: r#"<!-- bodymatter:v1 <page number="1" origin="native"/> -->"#,
            expected_tag: "page",
            expected_attrs: &[("number", "1"), ("origin", "native")],
        },
        MarkerCase {
            marker: BodymatterMarker::Page {
                number: 2,
                label: Some(""),
                origin: "ocr",
                partial: true,
            },
            expected_line: r#"<!-- bodymatter:v1 <page number="2" origin="ocr" completeness="partial"/> -->"#,
            expected_tag: "page",
            expected_attrs: &[
                ("number", "2"),
                ("origin", "ocr"),
                ("completeness", "partial"),
            ],
        },
        MarkerCase {
            marker: BodymatterMarker::Page {
                number: 4,
                label: Some("iv"),
                origin: "mixed",
                partial: true,
            },
            expected_line: r#"<!-- bodymatter:v1 <page number="4" label="iv" origin="mixed" completeness="partial"/> -->"#,
            expected_tag: "page",
            expected_attrs: &[
                ("number", "4"),
                ("label", "iv"),
                ("origin", "mixed"),
                ("completeness", "partial"),
            ],
        },
        MarkerCase {
            marker: BodymatterMarker::Sheet {
                number: 1,
                name: Some("Summary"),
            },
            expected_line: r#"<!-- bodymatter:v1 <sheet number="1" name="Summary"/> -->"#,
            expected_tag: "sheet",
            expected_attrs: &[("number", "1"), ("name", "Summary")],
        },
        MarkerCase {
            marker: BodymatterMarker::Sheet {
                number: 2,
                name: None,
            },
            expected_line: r#"<!-- bodymatter:v1 <sheet number="2"/> -->"#,
            expected_tag: "sheet",
            expected_attrs: &[("number", "2")],
        },
        MarkerCase {
            marker: BodymatterMarker::Sheet {
                number: 3,
                name: Some(""),
            },
            expected_line: r#"<!-- bodymatter:v1 <sheet number="3"/> -->"#,
            expected_tag: "sheet",
            expected_attrs: &[("number", "3")],
        },
        MarkerCase {
            marker: BodymatterMarker::Slide { number: 7 },
            expected_line: r#"<!-- bodymatter:v1 <slide number="7"/> -->"#,
            expected_tag: "slide",
            expected_attrs: &[("number", "7")],
        },
        MarkerCase {
            marker: BodymatterMarker::Table { number: 3 },
            expected_line: r#"<!-- bodymatter:v1 <table number="3"/> -->"#,
            expected_tag: "table",
            expected_attrs: &[("number", "3")],
        },
        MarkerCase {
            marker: BodymatterMarker::Chapter {
                number: 1,
                href: Some("Text/ch01.xhtml"),
            },
            expected_line: r#"<!-- bodymatter:v1 <chapter number="1" href="Text/ch01.xhtml"/> -->"#,
            expected_tag: "chapter",
            expected_attrs: &[("number", "1"), ("href", "Text/ch01.xhtml")],
        },
        MarkerCase {
            marker: BodymatterMarker::Chapter {
                number: 2,
                href: None,
            },
            expected_line: r#"<!-- bodymatter:v1 <chapter number="2"/> -->"#,
            expected_tag: "chapter",
            expected_attrs: &[("number", "2")],
        },
        MarkerCase {
            marker: BodymatterMarker::Chapter {
                number: 3,
                href: Some(""),
            },
            expected_line: r#"<!-- bodymatter:v1 <chapter number="3"/> -->"#,
            expected_tag: "chapter",
            expected_attrs: &[("number", "3")],
        },
        MarkerCase {
            marker: BodymatterMarker::PageBreak { number: 5 },
            expected_line: r#"<!-- bodymatter:v1 <page-break number="5"/> -->"#,
            expected_tag: "page-break",
            expected_attrs: &[("number", "5")],
        },
        MarkerCase {
            marker: BodymatterMarker::Page {
                number: 5_000_000_000_u64,
                label: None,
                origin: "native",
                partial: false,
            },
            expected_line: r#"<!-- bodymatter:v1 <page number="5000000000" origin="native"/> -->"#,
            expected_tag: "page",
            expected_attrs: &[("number", "5000000000"), ("origin", "native")],
        },
    ];

    for case in cases {
        let rendered = format_marker(&case.marker);
        assert_eq!(rendered, case.expected_line);
        let (tag, attrs) = parse_marker_line(&rendered);
        assert_eq!(tag, case.expected_tag);
        let expected_owned: Vec<(String, String)> = case
            .expected_attrs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(attrs, expected_owned);
    }
}

/// Verifies lossless XML round-trip and `--` prohibition across hostile attribute payloads.
#[test]
fn hostile_attribute_values_round_trip_losslessly_without_double_hyphens() {
    let hostile_values = [
        "Q1--Draft",
        "a-->b",
        "--",
        "---",
        "----",
        "-----",
        "-",
        "trailing-",
        "-leading",
        "<!-- bodymatter:v1 <page number=\"99\" origin=\"native\"/> -->",
        r#"<x attr="1">&"#,
        "'single'",
        "line1\nline2\r\nline3\rline4",
        "col1\tcol2",
        "&#45;&#45;&amp;&lt;&gt;&quot;",
        "日本語 📊 café — naïve",
    ];

    for raw in hostile_values {
        let escaped = escape_attr(raw);
        assert!(
            !escaped.contains("--"),
            "escaped attribute must never contain `--`: raw={raw:?}, escaped={escaped:?}"
        );
        assert!(
            !escaped.contains('\n') && !escaped.contains('\r'),
            "escaped attribute must never contain literal line breaks: raw={raw:?}, escaped={escaped:?}"
        );

        let page = format_marker(&BodymatterMarker::Page {
            number: 1,
            label: Some(raw),
            origin: "native",
            partial: false,
        });
        let (tag, attrs) = parse_marker_line(&page);
        assert_eq!(tag, "page");
        assert_eq!(
            attrs,
            vec![
                ("number".into(), "1".into()),
                ("label".into(), raw.into()),
                ("origin".into(), "native".into()),
            ]
        );

        let sheet = format_marker(&BodymatterMarker::Sheet {
            number: 2,
            name: Some(raw),
        });
        let (tag, attrs) = parse_marker_line(&sheet);
        assert_eq!(tag, "sheet");
        assert_eq!(
            attrs,
            vec![("number".into(), "2".into()), ("name".into(), raw.into())]
        );

        let chapter = format_marker(&BodymatterMarker::Chapter {
            number: 3,
            href: Some(raw),
        });
        let (tag, attrs) = parse_marker_line(&chapter);
        assert_eq!(tag, "chapter");
        assert_eq!(
            attrs,
            vec![("number".into(), "3".into()), ("href".into(), raw.into())]
        );

        let hostile_origin = format_marker(&BodymatterMarker::Page {
            number: 4,
            label: None,
            origin: raw,
            partial: false,
        });
        let (tag, attrs) = parse_marker_line(&hostile_origin);
        assert_eq!(tag, "page");
        assert_eq!(
            attrs,
            vec![("number".into(), "4".into()), ("origin".into(), raw.into())]
        );
    }

    assert_eq!(escape_attr("--"), "&#45;&#45;");
    assert_eq!(escape_attr("---"), "&#45;&#45;-");
    assert_eq!(escape_attr("----"), "&#45;&#45;&#45;&#45;");
    assert_eq!(escape_attr("a-->b"), "a&#45;&#45;&gt;b");
}

/// Verifies `escape_attr` enforces XML 1.0 §2.2 legality and escapes Unicode line terminators.
#[test]
fn escape_attr_enforces_xml_1_0_legality_and_escapes_unicode_line_terminators() {
    // Unicode line terminators must escape losslessly and never emit raw line breaks.
    assert_eq!(escape_attr("a\u{0085}b"), "a&#133;b");
    assert_eq!(escape_attr("a\u{2028}b"), "a&#8232;b");
    assert_eq!(escape_attr("a\u{2029}b"), "a&#8233;b");

    let unicode_breaks = "head\u{0085}nel\u{2028}ls\u{2029}ps";
    let marker = format_marker(&BodymatterMarker::Sheet {
        number: 1,
        name: Some(unicode_breaks),
    });
    assert!(
        !marker.chars().any(is_line_terminator),
        "emitted marker must contain zero raw line terminators: {marker:?}"
    );
    assert!(
        marker.chars().all(is_xml_1_0_char),
        "emitted marker must contain only XML 1.0 legal scalars: {marker:?}"
    );
    let (tag, attrs) = parse_marker_line(&marker);
    assert_eq!(tag, "sheet");
    assert_eq!(
        attrs,
        vec![
            ("number".into(), "1".into()),
            ("name".into(), unicode_breaks.into()),
        ]
    );

    // Every XML 1.0 §2.2 forbidden scalar (NUL/C0 except TAB/LF/CR, plus U+FFFE and U+FFFF)
    // must be sanitized to U+FFFD so emitted markers stay strictly XML 1.0 legal.
    let mut forbidden_scalars: Vec<char> = (0x0000_u32..=0x001F)
        .filter(|code| !matches!(code, 0x09 | 0x0A | 0x0D))
        .map(|code| char::from_u32(code).unwrap())
        .collect();
    forbidden_scalars.push('\u{FFFE}');
    forbidden_scalars.push('\u{FFFF}');

    for bad in forbidden_scalars {
        let raw = format!("pre{bad}post");
        let escaped = escape_attr(&raw);
        assert_eq!(
            escaped, "pre\u{FFFD}post",
            "XML 1.0 illegal scalar U+{:04X} must map to U+FFFD",
            bad as u32
        );
        assert!(escaped.chars().all(is_xml_1_0_char));
        assert!(!escaped.chars().any(is_line_terminator));

        let marker = format_marker(&BodymatterMarker::Page {
            number: 1,
            label: Some(&raw),
            origin: "native",
            partial: false,
        });
        assert!(
            marker.chars().all(is_xml_1_0_char),
            "marker for U+{:04X} contained XML 1.0 illegal scalar: {marker:?}",
            bad as u32
        );
        assert!(
            !marker.chars().any(is_line_terminator),
            "marker for U+{:04X} contained raw line terminator: {marker:?}",
            bad as u32
        );
        let (tag, attrs) = parse_marker_line(&marker);
        assert_eq!(tag, "page");
        assert_eq!(
            attrs,
            vec![
                ("number".into(), "1".into()),
                ("label".into(), "pre\u{FFFD}post".into()),
                ("origin".into(), "native".into()),
            ]
        );
    }

    // Combined hostile payload mixing NUL, C0 controls, noncharacters, and Unicode line breaks.
    let combined = "A\u{0000}\u{000B}\u{000C}\u{001F}\u{FFFE}\u{FFFF}\u{0085}\u{2028}\u{2029}B";
    let combined_marker = format_marker(&BodymatterMarker::Chapter {
        number: 9,
        href: Some(combined),
    });
    let (tag, attrs) = parse_marker_line(&combined_marker);
    assert_eq!(tag, "chapter");
    assert_eq!(
        attrs,
        vec![
            ("number".into(), "9".into()),
            (
                "href".into(),
                "A\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{0085}\u{2028}\u{2029}B".into()
            ),
        ]
    );
}

/// Verifies forged markers in body text and table cells are neutralized and never match the consumer pattern.
#[test]
fn forged_markers_in_body_and_table_cells_yield_zero_marker_matches() {
    let forged = r#"<!-- bodymatter:v1 <page number="99" origin="native"/> -->"#;

    let body_input =
        format!("Intro line\n{forged}\n<<!-- nested\n<!--<!-- double\n> {forged}\nOutro line");
    let sanitized = sanitize_body(&body_input);
    assert!(
        !sanitized.contains("<!--"),
        "sanitized body must contain zero literal `<!--`: {sanitized:?}"
    );
    assert!(
        sanitized.contains(r#"&lt;!-- bodymatter:v1 &lt;page number="99" origin="native"/> -->"#)
    );
    for line in sanitized.lines() {
        assert!(
            extract_marker_xml(line).is_none(),
            "forged body line unexpectedly matched marker grammar: {line:?}"
        );
    }

    assert_eq!(
        sanitize_body(forged),
        r#"&lt;!-- bodymatter:v1 &lt;page number="99" origin="native"/> -->"#
    );

    let rows = vec![
        ContentRow {
            row_index: 1,
            values: vec!["Header".into(), forged.into()],
        },
        ContentRow {
            row_index: 2,
            values: vec![forged.into(), "normal".into()],
        },
    ];
    let table = render_gfm_table(&rows);
    assert!(
        !table.contains("<!--"),
        "rendered GFM table must contain zero literal `<!--`: {table:?}"
    );
    for line in table.lines() {
        assert!(
            extract_marker_xml(line).is_none(),
            "forged table line unexpectedly matched marker grammar: {line:?}"
        );
    }
}

/// Verifies minimal line-leading Markdown escaping avoids spurious backslashes in ordinary prose.
#[test]
fn sanitize_body_escapes_only_line_leading_markdown_block_triggers() {
    let clean_prose = "The 2026-11-23 report for a_b & C++ is 100% ready (see [note]).\nfoo_bar and a * b and foo[0]";
    assert_eq!(sanitize_body(clean_prose), clean_prose);

    // Non-block leading characters must remain unescaped.
    let non_triggers = [
        "-5% YoY decline",
        "+12% growth",
        "--verbose flag",
        "**bold** start",
        "*italic* start",
        "_under_ start",
        "1.5 million units",
        "#hashtag without space",
        "a = b",
        "=SUM(A1:A5)",
        "= =",
        "    # four-space indented code block",
        "    ===",
    ];
    for line in non_triggers {
        assert_eq!(
            sanitize_body(line),
            line,
            "non-trigger line should stay unescaped: {line:?}"
        );
    }

    // Line-leading block triggers (0..=3 leading spaces) must be escaped.
    let triggers = [
        ("# Heading", r"\# Heading"),
        ("## Subheading", r"\## Subheading"),
        ("###### H6", r"\###### H6"),
        ("   # Indented heading", r"   \# Indented heading"),
        ("> quote", r"\> quote"),
        (">tight quote", r"\>tight quote"),
        ("  > indented quote", r"  \> indented quote"),
        ("=", r"\="),
        ("=====", r"\====="),
        ("===   \t", "\\===   \t"),
        ("   ===", r"   \==="),
        ("- item", r"\- item"),
        ("-", r"\-"),
        ("---", r"\---"),
        ("--", r"\--"),
        ("- - -", r"\- - -"),
        ("  - nested", r"  \- nested"),
        ("+ item", r"\+ item"),
        ("+", r"\+"),
        ("* item", r"\* item"),
        ("*", r"\*"),
        ("***", r"\***"),
        ("* * *", r"\* * *"),
        ("___", r"\___"),
        ("_ _ _", r"\_ _ _"),
        ("1. item", r"1\. item"),
        ("2) item", r"2\) item"),
        ("10.", r"10\."),
        ("   42) indented", r"   42\) indented"),
        ("```rust", r"\```rust"),
        ("```", r"\```"),
        ("~~~", r"\~~~"),
        ("~~~~yaml", r"\~~~~yaml"),
    ];
    for (input, expected) in triggers {
        assert_eq!(
            sanitize_body(input),
            expected,
            "unexpected escaping for trigger line {input:?}"
        );
    }

    // Setext H1 underline regression: escapes `=` underline while leaving inline `=` prose intact.
    assert_eq!(
        sanitize_body("Total Revenue\n=====\na = b"),
        "Total Revenue\n\\=====\na = b"
    );

    // CR and CRLF line endings normalize cleanly in a single pass with comment neutralization.
    assert_eq!(sanitize_body(""), "");
    assert_eq!(
        sanitize_body("first\r\n# second\r> third\n"),
        "first\n\\# second\n\\> third\n"
    );
    assert_eq!(sanitize_body("a\r\r\nb\r\n"), "a\n\nb\n");
    assert_eq!(
        sanitize_body("> <!-- one --> <!-- two -->\r\n<!-- three -->\r日本語\r\n====="),
        "\\> &lt;!-- one --> &lt;!-- two -->\n&lt;!-- three -->\n日本語\n\\====="
    );
}

/// Verifies GFM table rendering across empty, header-only, uneven, and hostile-cell inputs.
#[test]
fn render_gfm_table_handles_headers_uneven_rows_and_hostile_cells() {
    assert_eq!(render_gfm_table(&[]), "");

    let single_empty = vec![ContentRow {
        row_index: 1,
        values: vec![],
    }];
    assert_eq!(render_gfm_table(&single_empty), "|  |\n| --- |");

    let header_only = vec![ContentRow {
        row_index: 1,
        values: vec!["Region".into(), "Revenue".into()],
    }];
    assert_eq!(
        render_gfm_table(&header_only),
        "| Region | Revenue |\n| --- | --- |"
    );

    let uneven = vec![
        ContentRow {
            row_index: 1,
            values: vec!["Col A".into(), "Col B".into()],
        },
        ContentRow {
            row_index: 2,
            values: vec!["1".into(), "2".into(), "3".into()],
        },
        ContentRow {
            row_index: 3,
            values: vec!["only".into()],
        },
    ];
    assert_eq!(
        render_gfm_table(&uneven),
        concat!(
            "| Col A | Col B |  |\n",
            "| --- | --- | --- |\n",
            "| 1 | 2 | 3 |\n",
            "| only |  |  |",
        )
    );

    let hostile = vec![
        ContentRow {
            row_index: 1,
            values: vec!["Pipe | Header".into(), "Lines & Tabs".into()],
        },
        ContentRow {
            row_index: 2,
            values: vec![
                r"a|b and c\|d and e\\|f and C:\Users\Frank".into(),
                "r1\r\nr2\nr3\rr4\ttab <!-- x -->".into(),
            ],
        },
    ];
    let rendered = render_gfm_table(&hostile);
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], r"| Pipe \| Header | Lines & Tabs |");
    assert_eq!(lines[1], "| --- | --- |");
    assert_eq!(
        lines[2],
        r"| a\|b and c\\\|d and e\\\\\|f and C:\Users\Frank | r1<br>r2<br>r3<br>r4<br>tab &lt;!-- x --> |"
    );
    for line in &lines {
        assert_eq!(
            count_unescaped_pipes(line),
            3,
            "every 2-column GFM row must have exactly 3 unescaped pipes: {line:?}"
        );
    }

    assert_eq!(escape_table_cell("a|b"), r"a\|b");
    assert_eq!(escape_table_cell(r"a\|b"), r"a\\\|b");
    assert_eq!(escape_table_cell(r"C:\tmp\file"), r"C:\tmp\file");
}

/// Verifies `sanitize_body` and `escape_table_cell` neutralize raw source HTML
/// tags independently of generated `bodymatter:v1` comments and table `<br>`
/// breaks without introducing spurious backslashes in ordinary prose (GB-AUD-08).
#[test]
fn raw_source_html_is_escaped_in_body_and_table_cells_independently_of_generated_markup() {
    let raw_body = concat!(
        "Plain prose with a_b & C++.\n",
        "<script>alert(1)</script>\n",
        "<img src=x onerror=alert(1)>\n",
        "<div>block</div>\n",
        "<!-- bodymatter:v1 <page number=\"99\" origin=\"native\"/> -->",
    );
    let sanitized = sanitize_body(raw_body);
    assert!(sanitized.contains("Plain prose with a_b & C++."));
    assert!(!sanitized.contains("<script"));
    assert!(!sanitized.contains("<img"));
    assert!(!sanitized.contains("<div"));
    assert!(!sanitized.contains("<!--"));
    assert!(sanitized.contains("&lt;script>alert(1)&lt;/script>"));
    assert!(sanitized.contains("&lt;img src=x onerror=alert(1)>"));
    assert!(sanitized.contains("&lt;div>block&lt;/div>"));
    assert!(
        sanitized.contains("&lt;!-- bodymatter:v1 &lt;page number=\"99\" origin=\"native\"/> -->")
    );

    // Generated `<br>` from newlines/tabs survives while literal source `<br>` and HTML tags escape.
    let cell = escape_table_cell("<script>x</script>\n<img onerror=1>\t<br><!-- forged -->");
    assert_eq!(
        cell,
        "&lt;script>x&lt;/script><br>&lt;img onerror=1><br>&lt;br>&lt;!-- forged -->"
    );
    assert!(!cell.contains("<script"));
    assert!(!cell.contains("<img"));
    assert!(!cell.contains("<!--"));
    assert_eq!(cell.matches("<br>").count(), 2);

    // Generated markers are never passed through `sanitize_body` and remain intact.
    let marker = format_marker(&BodymatterMarker::Page {
        number: 1,
        label: None,
        origin: "native",
        partial: false,
    });
    assert_eq!(
        marker,
        r#"<!-- bodymatter:v1 <page number="1" origin="native"/> -->"#
    );
}

/// Verifies `sanitize_body` normalizes Unicode/vertical-tab/form-feed line
/// terminators to `\n` before block-trigger escaping, replaces forbidden C0
/// controls (including ANSI ESC) and noncharacters with `U+FFFD`, and preserves
/// `\t` in prose (`T02-SEC-002`).
#[test]
fn sanitize_body_normalizes_unicode_line_terminators_and_replaces_forbidden_controls() {
    // Non-LF line terminators normalize to `\n` so smuggled leading block triggers are escaped.
    assert_eq!(sanitize_body("ok\u{2028}# heading"), "ok\n\\# heading");
    assert_eq!(sanitize_body("ok\u{0085}> quote"), "ok\n\\> quote");
    assert_eq!(sanitize_body("ok\u{2029}---"), "ok\n\\---");
    assert_eq!(sanitize_body("ok\u{000B}- item"), "ok\n\\- item");
    assert_eq!(sanitize_body("ok\u{000C}```rust"), "ok\n\\```rust");
    assert_eq!(
        sanitize_body("a\u{000B}b\u{000C}c\u{0085}d\u{2028}e\u{2029}f\r\ng\rh\ni"),
        "a\nb\nc\nd\ne\nf\ng\nh\ni"
    );

    // Tabs remain preserved as ordinary whitespace in prose.
    assert_eq!(
        sanitize_body("col1\tcol2\n\tindented"),
        "col1\tcol2\n\tindented"
    );

    // ANSI escape sequences are neutralized via U+FFFD replacement.
    assert_eq!(
        sanitize_body("\x1b[31mred\x1b[0m"),
        "\u{FFFD}[31mred\u{FFFD}[0m"
    );

    // Every forbidden C0 control (0x00..=0x08, 0x0E..=0x1F) and U+FFFE/U+FFFF maps to U+FFFD.
    let mut forbidden_scalars: Vec<char> = (0x0000_u32..=0x001F)
        .filter(|code| !matches!(code, 0x09..=0x0D))
        .map(|code| char::from_u32(code).unwrap())
        .collect();
    forbidden_scalars.push('\u{FFFE}');
    forbidden_scalars.push('\u{FFFF}');

    for bad in forbidden_scalars {
        let raw = format!("pre{bad}post");
        let sanitized = sanitize_body(&raw);
        assert_eq!(
            sanitized, "pre\u{FFFD}post",
            "forbidden scalar U+{:04X} must map to U+FFFD in sanitize_body",
            bad as u32
        );
        assert!(
            !sanitized.chars().any(is_line_terminator),
            "unexpected line terminator for U+{:04X}: {sanitized:?}",
            bad as u32
        );
    }

    // Combined payload with HTML, ANSI ESC, NUL/BEL/US, noncharacters, and smuggled heading.
    let combined = "<script>\x1b[31m\u{0000}\u{0007}\u{001F}\u{FFFE}\u{FFFF}\u{2028}# smuggled";
    let sanitized = sanitize_body(combined);
    assert_eq!(
        sanitized,
        "&lt;script>\u{FFFD}[31m\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\n\\# smuggled"
    );
    for ch in sanitized.chars() {
        if ch != '\n' {
            assert!(
                !is_line_terminator(ch),
                "raw non-LF line terminator survived sanitize_body: U+{:04X}",
                ch as u32
            );
        }
    }
}

/// Verifies `escape_table_cell` maps all Unicode/ASCII line terminators and tabs
/// to `<br>`, replaces forbidden C0 controls (including ANSI ESC) and
/// noncharacters with `U+FFFD`, and keeps every GFM table cell strictly
/// single-line with zero raw C0 controls (`T02-SEC-002`).
#[test]
fn escape_table_cell_neutralizes_unicode_line_terminators_and_forbidden_controls() {
    let all_breaks = "a\r\nb\rc\nd\te\u{000B}f\u{000C}g\u{0085}h\u{2028}i\u{2029}j";
    let escaped_breaks = escape_table_cell(all_breaks);
    assert_eq!(
        escaped_breaks,
        "a<br>b<br>c<br>d<br>e<br>f<br>g<br>h<br>i<br>j"
    );
    assert!(!escaped_breaks.chars().any(is_line_terminator));
    assert!(!escaped_breaks.chars().any(|ch| ch <= '\u{001F}'));

    assert_eq!(
        escape_table_cell("\x1b[31mred\x1b[0m"),
        "\u{FFFD}[31mred\u{FFFD}[0m"
    );

    // Exhaustively verify all C0 controls (0x00..=0x1F), Unicode breaks, and U+FFFE/U+FFFF.
    for code in 0x0000_u32..=0x001F {
        let ch = char::from_u32(code).unwrap();
        let escaped = escape_table_cell(&format!("x{ch}y"));
        let expected = if matches!(code, 0x09..=0x0D) {
            "x<br>y"
        } else {
            "x\u{FFFD}y"
        };
        assert_eq!(
            escaped, expected,
            "unexpected escape_table_cell output for C0 U+{code:04X}"
        );
        assert!(
            !escaped.chars().any(is_line_terminator),
            "raw line terminator survived escape_table_cell for U+{code:04X}: {escaped:?}"
        );
        assert!(
            !escaped.chars().any(|c| c <= '\u{001F}'),
            "raw C0 control survived escape_table_cell for U+{code:04X}: {escaped:?}"
        );
    }

    for ch in ['\u{0085}', '\u{2028}', '\u{2029}'] {
        let escaped = escape_table_cell(&format!("x{ch}y"));
        assert_eq!(escaped, "x<br>y");
        assert!(!escaped.chars().any(is_line_terminator));
    }
    for ch in ['\u{FFFE}', '\u{FFFF}'] {
        let escaped = escape_table_cell(&format!("x{ch}y"));
        assert_eq!(escaped, "x\u{FFFD}y");
    }

    // Combined hostile table row keeps backslash-before-pipe intact and single-line framing strict.
    let rows = vec![
        ContentRow {
            row_index: 1,
            values: vec!["Col1".into(), "Col2".into()],
        },
        ContentRow {
            row_index: 2,
            values: vec![
                "a\\|b\u{2028}\x1b[31mred\x1b[0m".into(),
                "<tag>\u{0085}\u{000B}\u{000C}\u{2029}\u{0000}\u{0007}\u{001F}\u{FFFE}\u{FFFF}"
                    .into(),
            ],
        },
    ];
    let table = render_gfm_table(&rows);
    let lines: Vec<&str> = table.split('\n').collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[2],
        r"| a\\\|b<br>\u{FFFD}[31mred\u{FFFD}[0m | &lt;tag><br><br><br><br>\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD} |"
            .replace(r"\u{FFFD}", "\u{FFFD}")
    );
    for line in &lines {
        assert!(!line.chars().any(is_line_terminator));
        assert!(!line.chars().any(|ch| ch <= '\u{001F}'));
        assert_eq!(count_unescaped_pipes(line), 3);
    }
}
