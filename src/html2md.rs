/// Conservative HTML -> Markdown. Entities decoded, block tags to newlines,
/// remaining tags stripped. Enough for README.md context; real converter only
/// when a page renders badly.
pub fn html_to_md(html: &str) -> String {
    let mut s = html.to_string();
    for tag in [
        "<br>", "<br/>", "<br />", "</p>", "</div>", "</li>", "</h1>", "</h2>", "</h3>", "</h4>",
    ] {
        s = s.replace(tag, "\n");
    }
    s = s.replace("<li>", "- ");
    // strip remaining tags
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&out);
    // collapse 3+ newlines to 2
    let mut collapsed = String::with_capacity(decoded.len());
    let mut nl = 0;
    for ch in decoded.chars() {
        if ch == '\n' {
            nl += 1;
            if nl <= 2 {
                collapsed.push(ch);
            }
        } else {
            nl = 0;
            collapsed.push(ch);
        }
    }
    collapsed.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_and_keeps_text() {
        assert_eq!(html_to_md("<p>Hola <b>mon</b></p>"), "Hola mon");
        assert_eq!(html_to_md("<ul><li>a</li><li>b</li></ul>"), "- a\n- b");
        assert_eq!(html_to_md("A &amp; B&nbsp;C"), "A & B C");
    }
}
