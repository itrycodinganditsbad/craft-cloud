//! The pages' building blocks: escaping, the layout, and the form fields.
//!
//! Pages are plain `format!`ed HTML. The rule that keeps them safe is small: **every value that is not a literal in this
//! code goes through [`esc`]** before it is put into markup, and the helpers here escape their own arguments, so a page
//! built from them has no place to forget it. There is no script, no inline style and no external file anywhere (the
//! CSP, see [`super::secure_headers`], forbids all three), so a value that did slip through could still not run code.

use std::fmt::Write;

/// `text` escaped for HTML text and for attribute values in double or single quotes.
pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // Control characters other than the usual whitespace have no place in a page.
            c if c.is_control() && !matches!(c, '\n' | '\t') => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// A time in milliseconds since the Unix epoch as `2026-11-04 13:05:09 UTC`, or `-` for one that is out of range.
pub fn fmt_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm), in checked arithmetic.
    let date = || -> Option<(i64, i64, i64)> {
        let z = days.checked_add(719_468)?;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe.checked_add(era.checked_mul(400)?)?.checked_add(i64::from(month <= 2))?;
        Some((year, month, day))
    };
    match date() {
        Some((y, m, d)) if (0..=9999).contains(&y) => format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", rest / 3600, rest % 3600 / 60, rest % 60),
        _ => "-".to_owned(),
    }
}

/// A size in bytes as `1.5 GiB`, exact below one KiB.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    // Tenths of the unit, in 128 bits so that nothing overflows.
    let mut tenths = u128::from(bytes) * 10 / 1024;
    let mut unit = 0usize;
    while tenths >= 10_240 && unit + 1 < UNITS.len() {
        tenths /= 1024;
        unit += 1;
    }
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS.get(unit).copied().unwrap_or("PiB"))
}

/// What a page shows about the signed-in admin.
pub struct Ctx {
    pub username: String,
    /// The CSRF token the forms of this session carry.
    pub csrf: String,
}

/// The whole page: `body` is markup built with this module's helpers, `title` plain text.
pub fn page(ctx: Option<&Ctx>, title: &str, body: &str) -> String {
    let nav = match ctx {
        Some(ctx) => format!(
            "<nav aria-label=\"Admin\"><ul><li><a href=\"/web\">Overview</a></li><li><a href=\"/web/users\">Users</a></li>\
             <li><a href=\"/web/spaces\">Team spaces</a></li><li><a href=\"/web/audit\">Audit log</a></li></ul>\
             <form method=\"post\" action=\"/web/logout\" class=\"inline\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">\
             <span>Signed in as <strong>{}</strong></span> <button type=\"submit\">Sign out</button></form></nav>",
            esc(&ctx.csrf),
            esc(&ctx.username)
        ),
        None => String::new(),
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex, nofollow\"><title>{} - Craft Cloud admin</title>\
         <link rel=\"stylesheet\" href=\"/web/style.css\"></head><body><a class=\"skip\" href=\"#main\">Skip to the content</a>\
         <header><p class=\"brand\">Craft Cloud admin</p>{nav}</header><main id=\"main\" tabindex=\"-1\">{body}</main></body></html>\n",
        esc(title)
    )
}

/// A page heading and its first paragraph.
pub fn heading(text: &str) -> String {
    format!("<h1>{}</h1>", esc(text))
}

/// A notice that something worked (`role=status`) or failed (`role=alert`).
pub fn notice(text: &str, error: bool) -> String {
    let (class, role) = if error { ("notice error", "alert") } else { ("notice", "status") };
    format!("<p class=\"{class}\" role=\"{role}\">{}</p>", esc(text))
}

/// A link to a path made by this code (never to a value from the outside as it is).
pub fn link(href: &str, text: &str) -> String {
    format!("<a href=\"{}\">{}</a>", esc(href), esc(text))
}

/// Percent-encoding for a value that goes into a query string.
pub fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A labelled text field. `kind` is an input type from this code (`text`, `password`, `search`, `number`).
pub fn field(form: &str, name: &str, label: &str, kind: &str, value: &str, extra: &str) -> String {
    let id = esc(&format!("{form}-{name}"));
    format!(
        "<p class=\"field\"><label for=\"{id}\">{}</label><input id=\"{id}\" name=\"{}\" type=\"{kind}\" value=\"{}\" {extra}></p>",
        esc(label),
        esc(name),
        esc(value)
    )
}

/// A labelled select; `options` are `(value, text)` pairs and `selected` the value that is chosen.
pub fn select(form: &str, name: &str, label: &str, options: &[(&str, &str)], selected: &str) -> String {
    let id = esc(&format!("{form}-{name}"));
    let mut out = format!("<p class=\"field\"><label for=\"{id}\">{}</label><select id=\"{id}\" name=\"{}\">", esc(label), esc(name));
    for (value, text) in options {
        let chosen = if *value == selected { " selected" } else { "" };
        let _ = write!(out, "<option value=\"{}\"{chosen}>{}</option>", esc(value), esc(text));
    }
    out.push_str("</select></p>");
    out
}

/// A labelled checkbox.
pub fn checkbox(form: &str, name: &str, label: &str) -> String {
    let id = esc(&format!("{form}-{name}"));
    format!(
        "<p class=\"field check\"><input id=\"{id}\" name=\"{}\" type=\"checkbox\" value=\"on\"><label for=\"{id}\">{}</label></p>",
        esc(name),
        esc(label)
    )
}

/// The confirmation every change asks for: the admin's password and, when their account has two-factor sign-in, a code or a
/// recovery code. The server asks again whatever the page showed.
pub fn confirm_fields(form: &str, two_factor: bool) -> String {
    let mut out =
        field(form, "password", "Your password, to confirm", "password", "", "required autocomplete=\"current-password\" maxlength=\"1024\"");
    if two_factor {
        out.push_str(&field(
            form,
            "totp_code",
            "Code from your authenticator app",
            "text",
            "",
            "inputmode=\"numeric\" autocomplete=\"one-time-code\" pattern=\"[0-9]{6}\" maxlength=\"6\"",
        ));
        out.push_str(&field(form, "recovery_code", "Or a recovery code", "text", "", "autocomplete=\"off\" maxlength=\"32\" spellcheck=\"false\""));
    }
    out
}

/// A form that posts to `action` with the session's CSRF token, `inner` being markup built with the helpers above.
pub fn post_form(ctx: &Ctx, action: &str, class: &str, inner: &str) -> String {
    format!(
        "<form method=\"post\" action=\"{}\" class=\"{}\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">{inner}</form>",
        esc(action),
        esc(class),
        esc(&ctx.csrf)
    )
}

/// A table with a caption and column headers. Every cell is markup already built with [`esc`].
pub fn table(caption: &str, headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = format!("<table><caption>{}</caption><thead><tr>", esc(caption));
    for header in headers {
        let _ = write!(out, "<th scope=\"col\">{}</th>", esc(header));
    }
    out.push_str("</tr></thead><tbody>");
    for row in rows {
        out.push_str("<tr>");
        for (n, cell) in row.iter().enumerate() {
            if n == 0 {
                let _ = write!(out, "<th scope=\"row\">{cell}</th>");
            } else {
                let _ = write!(out, "<td>{cell}</td>");
            }
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_characters_are_escaped_everywhere_a_value_goes() {
        let nasty = "<script>alert(\"x\")</script>'&";
        assert_eq!(esc(nasty), "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;&#39;&amp;");
        assert_eq!(esc("a\u{0}b\u{7}c\nd"), "a\u{fffd}b\u{fffd}c\nd");
        for html in [
            heading(nasty),
            notice(nasty, true),
            link(nasty, nasty),
            field("f", nasty, nasty, "text", nasty, ""),
            select("f", nasty, nasty, &[(nasty, nasty)], nasty),
            checkbox("f", nasty, nasty),
            page(Some(&Ctx { username: nasty.to_owned(), csrf: nasty.to_owned() }), nasty, ""),
            post_form(&Ctx { username: String::new(), csrf: nasty.to_owned() }, nasty, nasty, ""),
            table(nasty, &[nasty], &[]),
        ] {
            assert!(!html.contains("<script>") && !html.contains("alert(\"x\")"), "{html}");
        }
    }

    #[test]
    fn times_and_sizes_read_well() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_time(1_760_000_000_000), "2025-10-09 08:53:20 UTC");
        assert_eq!(fmt_time(951_782_400_000 + 86_399_000), "2000-02-29 23:59:59 UTC");
        assert_eq!(fmt_time(-1), "1969-12-31 23:59:59 UTC");
        assert_eq!(fmt_time(i64::MAX), "-");
        assert_eq!(fmt_time(i64::MIN), "-");
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024), "3.5 GiB");
        assert_eq!(fmt_bytes(u64::MAX), "16383.9 PiB");
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(url_encode("auth.login-1_2~"), "auth.login-1_2~");
    }
}
