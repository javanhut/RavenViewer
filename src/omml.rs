//! Word equations (Office Math, `m:oMath`) as text: written the way they
//! would be on one line — variables in italics, subscripts and
//! superscripts raised and lowered, fractions with a slash, roots with √,
//! matrices row by row. Both as Pango markup, for drawing, and as plain
//! text (with Unicode sub- and superscripts where they exist), for
//! exporting to text and to Word 97–2003.
//!
//! This is for showing an equation; the equation itself is kept in the
//! document exactly as it was.

use quick_xml::Reader;
use quick_xml::events::Event;

#[derive(Debug, Default)]
struct Node {
    name: String,
    val: Option<String>,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    fn all(&self, name: &str) -> impl Iterator<Item = &Node> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The `m:val` of a property, as in `<m:dPr><m:begChr m:val="["/>`.
    fn prop(&self, props: &str, name: &str) -> Option<String> {
        self.child(props)?.child(name)?.val.clone()
    }
}

fn local(name: &[u8]) -> String {
    String::from_utf8_lossy(name.rsplit(|&b| b == b':').next().unwrap_or(name)).into_owned()
}

fn tree(xml: &str) -> Node {
    let mut reader = Reader::from_str(xml);
    let mut stack = vec![Node::default()];
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let val = e.attributes().flatten().find(|a| a.key.local_name().as_ref() == b"val").and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                stack.push(Node { name: local(e.name().as_ref()), val, ..Default::default() });
            }
            Ok(Event::Empty(e)) => {
                let val = e.attributes().flatten().find(|a| a.key.local_name().as_ref() == b"val").and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node { name: local(e.name().as_ref()), val, ..Default::default() });
                }
            }
            Ok(Event::Text(t)) => {
                if let (Some(top), Ok(text)) = (stack.last_mut(), t.unescape()) {
                    top.text.push_str(&text);
                }
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1
                    && let Some(done) = stack.pop()
                    && let Some(top) = stack.last_mut()
                {
                    top.children.push(done);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    // Whatever was left open closes with the equation.
    while stack.len() > 1 {
        if let Some(done) = stack.pop()
            && let Some(top) = stack.last_mut()
        {
            top.children.push(done);
        }
    }
    stack.pop().unwrap_or_default()
}

/// An equation as written on one line, plain and as markup.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Linear {
    pub plain: String,
    pub markup: String,
}

impl Linear {
    fn push(&mut self, other: Linear) {
        self.plain.push_str(&other.plain);
        self.markup.push_str(&other.markup);
    }

    fn text(s: &str) -> Linear {
        Linear { plain: s.to_string(), markup: escape(s) }
    }

    fn len(&self) -> usize {
        self.plain.chars().count()
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// An `m:oMath` or `m:oMathPara` element as text.
pub fn linear(xml: &str) -> Linear {
    let root = tree(xml);
    let mut out = Linear::default();
    for child in &root.children {
        out.push(node(child));
    }
    out
}

fn children(n: &Node) -> Linear {
    let mut out = Linear::default();
    for c in &n.children {
        out.push(node(c));
    }
    out
}

fn part(n: &Node, name: &str) -> Linear {
    n.child(name).map(children).unwrap_or_default()
}

fn script(base: Linear, sub: Option<Linear>, sup: Option<Linear>) -> Linear {
    let mut out = base;
    if let Some(s) = sub.filter(|s| s.len() > 0) {
        out.plain.push_str(&lowered(&s.plain));
        out.markup.push_str(&format!("<sub>{}</sub>", s.markup));
    }
    if let Some(s) = sup.filter(|s| s.len() > 0) {
        out.plain.push_str(&raised(&s.plain));
        out.markup.push_str(&format!("<sup>{}</sup>", s.markup));
    }
    out
}

/// In brackets, unless it is a single symbol.
fn grouped(l: Linear) -> Linear {
    if l.len() <= 1 {
        return l;
    }
    let mut out = Linear::text("(");
    out.push(l);
    out.push(Linear::text(")"));
    out
}

fn node(n: &Node) -> Linear {
    match n.name.as_str() {
        "r" => {
            // "Normal text" and plain style are upright.
            let plain = n.child("rPr").is_some_and(|p| p.child("nor").is_some() || p.child("sty").is_some_and(|s| s.val.as_deref() == Some("p")));
            let text: String = n.all("t").map(|t| t.text.as_str()).collect();
            let mut out = Linear { plain: text.clone(), markup: String::new() };
            // Letters are variables, set in italics; numbers and operators
            // are not.
            for c in text.chars() {
                let s = escape(&c.to_string());
                if c.is_alphabetic() && !plain {
                    out.markup.push_str(&format!("<i>{s}</i>"));
                } else {
                    out.markup.push_str(&s);
                }
            }
            out
        }
        "sSub" => script(part(n, "e"), Some(part(n, "sub")), None),
        "sSup" => script(part(n, "e"), None, Some(part(n, "sup"))),
        "sSubSup" => script(part(n, "e"), Some(part(n, "sub")), Some(part(n, "sup"))),
        "sPre" => {
            let mut out = script(Linear::default(), Some(part(n, "sub")), Some(part(n, "sup")));
            out.push(part(n, "e"));
            out
        }
        "f" => {
            let mut out = grouped(part(n, "num"));
            out.push(Linear::text("/"));
            out.push(grouped(part(n, "den")));
            out
        }
        "rad" => {
            let deg = part(n, "deg");
            let mut out = script(Linear::default(), None, Some(deg));
            out.push(Linear::text("√"));
            out.push(grouped(part(n, "e")));
            out
        }
        "d" => {
            let open = n.prop("dPr", "begChr").unwrap_or_else(|| "(".into());
            let close = n.prop("dPr", "endChr").unwrap_or_else(|| ")".into());
            let sep = n.prop("dPr", "sepChr").unwrap_or_else(|| "|".into());
            let mut out = Linear::text(&open);
            for (i, e) in n.all("e").enumerate() {
                if i > 0 {
                    out.push(Linear::text(&sep));
                }
                out.push(children(e));
            }
            out.push(Linear::text(&close));
            out
        }
        "nary" => {
            let op = n.prop("naryPr", "chr").unwrap_or_else(|| "∫".into());
            let hide = |name: &str| n.prop("naryPr", name).is_some_and(|v| v == "1" || v == "on" || v == "true");
            let sub = (!hide("subHide")).then(|| part(n, "sub"));
            let sup = (!hide("supHide")).then(|| part(n, "sup"));
            let mut out = script(Linear::text(&op), sub, sup);
            out.push(Linear::text(" "));
            out.push(part(n, "e"));
            out
        }
        "func" => {
            let mut name = part(n, "fName");
            // Function names are upright.
            name.markup = name.markup.replace("<i>", "").replace("</i>", "");
            name.push(Linear::text("\u{2009}"));
            name.push(part(n, "e"));
            name
        }
        "acc" => {
            let mark = n.prop("accPr", "chr").unwrap_or_else(|| "\u{0302}".into());
            let mut out = part(n, "e");
            out.plain.push_str(&mark);
            out.markup.push_str(&escape(&mark));
            out
        }
        "bar" => {
            let e = part(n, "e");
            Linear { markup: format!("<span overline=\"single\">{}</span>", e.markup), plain: e.plain }
        }
        "limLow" => script(part(n, "e"), Some(part(n, "lim")), None),
        "limUpp" => script(part(n, "e"), None, Some(part(n, "lim"))),
        "m" => {
            let mut out = Linear::default();
            for (i, row) in n.all("mr").enumerate() {
                if i > 0 {
                    out.push(Linear::text("; "));
                }
                for (j, e) in row.all("e").enumerate() {
                    if j > 0 {
                        out.push(Linear::text("  "));
                    }
                    out.push(children(e));
                }
            }
            out
        }
        "eqArr" => {
            let mut out = Linear::default();
            for (i, e) in n.all("e").enumerate() {
                if i > 0 {
                    out.push(Linear::text(";  "));
                }
                out.push(children(e));
            }
            out
        }
        "oMath" => children(n),
        "oMathPara" => {
            let mut out = Linear::default();
            for (i, m) in n.all("oMath").enumerate() {
                if i > 0 {
                    out.push(Linear::text("    "));
                }
                out.push(children(m));
            }
            out
        }
        // Properties say how to draw what is next to them, not what it is.
        name if name.ends_with("Pr") => Linear::default(),
        // Word's own run inside an equation.
        "t" => Linear::text(&n.text),
        _ => children(n),
    }
}

/// Text in Unicode subscripts, where there are some; otherwise `_(…)`.
fn lowered(s: &str) -> String {
    let map = |c: char| -> Option<char> {
        Some(match c {
            '0'..='9' => char::from_u32(0x2080 + c as u32 - '0' as u32)?,
            '+' => '₊',
            '-' | '−' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'o' => 'ₒ',
            'x' => 'ₓ',
            'h' => 'ₕ',
            'k' => 'ₖ',
            'l' => 'ₗ',
            'm' => 'ₘ',
            'n' => 'ₙ',
            'p' => 'ₚ',
            's' => 'ₛ',
            't' => 'ₜ',
            'i' => 'ᵢ',
            'j' => 'ⱼ',
            'r' => 'ᵣ',
            'u' => 'ᵤ',
            'v' => 'ᵥ',
            _ => return None,
        })
    };
    s.chars().map(map).collect::<Option<String>>().unwrap_or_else(|| format!("_({s})"))
}

/// Text in Unicode superscripts, where there are some; otherwise `^(…)`.
fn raised(s: &str) -> String {
    let map = |c: char| -> Option<char> {
        Some(match c {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4'..='9' => char::from_u32(0x2070 + c as u32 - '0' as u32)?,
            '+' => '⁺',
            '-' | '−' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            'n' => 'ⁿ',
            'i' => 'ⁱ',
            'T' => 'ᵀ',
            _ => return None,
        })
    };
    s.chars().map(map).collect::<Option<String>>().unwrap_or_else(|| format!("^({s})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = r#"xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math""#;

    #[test]
    fn equations_read_as_one_line() {
        // (W₁ × H₁) from the homework sheet, as Word writes it.
        let xml = format!(
            r#"<m:oMath {M}><m:d><m:dPr><m:ctrlPr/></m:dPr><m:e><m:sSub><m:e><m:r><m:t>W</m:t></m:r></m:e><m:sub><m:r><m:t>1</m:t></m:r></m:sub></m:sSub><m:r><m:t>×</m:t></m:r><m:sSub><m:e><m:r><m:t>H</m:t></m:r></m:e><m:sub><m:r><m:t>1</m:t></m:r></m:sub></m:sSub></m:e></m:d></m:oMath>"#
        );
        let l = linear(&xml);
        assert_eq!(l.plain, "(W₁×H₁)");
        assert_eq!(l.markup, "(<i>W</i><sub>1</sub>×<i>H</i><sub>1</sub>)");
        let frac = format!(r#"<m:oMath {M}><m:f><m:num><m:r><m:t>a+b</m:t></m:r></m:num><m:den><m:r><m:t>2</m:t></m:r></m:den></m:f><m:rad><m:radPr><m:degHide m:val="1"/></m:radPr><m:deg/><m:e><m:r><m:t>x</m:t></m:r></m:e></m:rad></m:oMath>"#);
        assert_eq!(linear(&frac).plain, "(a+b)/2√x");
        let sum = format!(r#"<m:oMath {M}><m:nary><m:naryPr><m:chr m:val="∑"/></m:naryPr><m:sub><m:r><m:t>i=1</m:t></m:r></m:sub><m:sup><m:r><m:t>n</m:t></m:r></m:sup><m:e><m:sSup><m:e><m:r><m:t>x</m:t></m:r></m:e><m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup></m:e></m:nary></m:oMath>"#);
        assert_eq!(linear(&sum).plain, "∑ᵢ₌₁ⁿ x²");
    }
}
