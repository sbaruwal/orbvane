use super::*;

fn html(abbr: &str) -> String {
    expand(abbr, Syntax::Html).unwrap_or_else(|| panic!("{abbr} didn't expand"))
}

fn css(abbr: &str) -> String {
    expand(abbr, Syntax::Css).unwrap_or_else(|| panic!("{abbr} didn't expand"))
}

#[test]
fn nests_repeats_and_numbers() {
    assert_eq!(html("ul>li*3"), "<ul>\n\t<li>${1}</li>\n\t<li>${2}</li>\n\t<li>${3}</li>\n</ul>");
    assert_eq!(html("h$*2"), "<h1>${1}</h1>\n<h2>${2}</h2>");
    assert_eq!(html("p{Item $$}*2"), "<p>Item 01</p>\n<p>Item 02</p>");
    assert_eq!(html("li.i$@-*3"), "<li class=\"i3\">${1}</li>\n<li class=\"i2\">${2}</li>\n<li class=\"i1\">${3}</li>");
    assert_eq!(html("div>p^span"), "<div>\n\t<p>${1}</p>\n</div>\n<span>${2}</span>");
    assert_eq!(html("(dt+dd)*2"), "<dt>${1}</dt>\n<dd>${2}</dd>\n<dt>${3}</dt>\n<dd>${4}</dd>");
    assert_eq!(html("div+p"), "<div>${1}</div>\n<p>${2}</p>");
}

#[test]
fn attributes_classes_and_implicit_tags() {
    assert_eq!(html("div.a.b#x"), "<div class=\"a b\" id=\"x\">${1}</div>");
    assert_eq!(html(".x"), "<div class=\"x\">${1}</div>");
    assert_eq!(html("ul>.x"), "<ul>\n\t<li class=\"x\">${1}</li>\n</ul>");
    assert_eq!(html("em>.x"), "<em><span class=\"x\">${1}</span></em>");
    assert_eq!(html("a"), "<a href=\"${1}\">${2}</a>");
    assert_eq!(html("a[href=/x title='A b' disabled.]{go}"), "<a href=\"/x\" title=\"A b\" disabled=\"disabled\">go</a>");
    assert_eq!(html("td[colspan=2]"), "<td colspan=\"2\">${1}</td>");
}

#[test]
fn snippets_resolve() {
    assert_eq!(html("img"), "<img src=\"${1}\" alt=\"${2}\">");
    assert_eq!(expand("img", Syntax::Jsx).unwrap(), "<img src=\"${1}\" alt=\"${2}\" />");
    assert_eq!(expand("br", Syntax::Xml).unwrap(), "<br/>");
    assert_eq!(html("input:t"), "<input type=\"text\" name=\"${1}\" id=\"${2}\">");
    assert_eq!(html("label>inp"), "<label><input type=\"${1:text}\" name=\"${2}\"></label>");
    assert_eq!(html("link:css"), "<link rel=\"stylesheet\" href=\"${1:style}.css\">");
    assert_eq!(
        html("!"),
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\t<meta charset=\"UTF-8\">\n\t\
         <meta name=\"viewport\" content=\"width=${1:device-width}, initial-scale=${2:1.0}\">\n\t\
         <title>${3:Document}</title>\n</head>\n<body>\n\t${4}\n</body>\n</html>"
    );
}

#[test]
fn inline_elements_stay_on_the_line() {
    assert_eq!(html("p>a+b"), "<p><a href=\"${1}\">${2}</a><b>${3}</b></p>");
    assert_eq!(html("p>i*3"), "<p>\n\t<i>${1}</i>\n\t<i>${2}</i>\n\t<i>${3}</i>\n</p>");
    assert_eq!(html("div>span"), "<div><span>${1}</span></div>");
}

#[test]
fn jsx_attributes() {
    let jsx = |a| expand(a, Syntax::Jsx).unwrap();
    assert_eq!(jsx("div.box"), "<div className=\"box\">${1}</div>");
    assert_eq!(jsx("label[for=x]"), "<label htmlFor=\"x\">${1}</label>");
    assert_eq!(jsx("button[onClick={go}]"), "<button onClick={go\\}>${1}</button>");
    assert_eq!(jsx("Foo.Bar"), "<Foo.Bar>${1}</Foo.Bar>");
}

#[test]
fn escapes_snippet_syntax() {
    assert_eq!(html("p{a $ b}"), "<p>a \\$ b</p>");
}

#[test]
fn lorem_ipsum() {
    let s = html("lorem5");
    assert!(s.starts_with("Lorem ipsum dolor sit amet"), "{s}");
    assert_eq!(s.split_whitespace().count(), 5);
    let s = html("p*2>lorem4");
    assert_eq!(s.matches("<p>").count(), 2);
    assert_eq!(s.matches("Lorem").count(), 1, "{s}");
}

#[test]
fn stylesheets() {
    assert_eq!(css("m10"), "margin: 10px;");
    assert_eq!(css("p10-20"), "padding: 10px 20px;");
    assert_eq!(css("m-10"), "margin: -10px;");
    assert_eq!(css("db"), "display: block;");
    assert_eq!(css("pos"), "position: ${1:relative};");
    assert_eq!(css("pos:a"), "position: absolute;");
    assert_eq!(css("c#f"), "color: #fff;");
    assert_eq!(css("c#f.5"), "color: rgba(255, 255, 255, 0.5);");
    assert_eq!(css("w100p"), "width: 100%;");
    assert_eq!(css("lh1.5"), "line-height: 1.5;");
    assert_eq!(css("m"), "margin: ${1};");
    assert_eq!(css("bd"), "border: ${1:1px} ${2:solid} ${3:#000};");
    assert_eq!(css("m10!"), "margin: 10px !important;");
    assert_eq!(css("m10+p5"), "margin: 10px;\npadding: 5px;");
    assert_eq!(css("@m"), "@media ${1:screen} {\n\t${2}\n\\}");
    assert_eq!(expand("zzq", Syntax::Css), None);
}

#[test]
fn finds_abbreviations_where_emmet_works() {
    let at = |syntax, before: &str, suggest| {
        let line = before.rsplit('\n').next().unwrap();
        at_caret(syntax, before, line, suggest).map(|e| (e.start, e.abbr))
    };
    assert_eq!(at(Syntax::Html, "<div>ul>li.a", true), Some((5, "ul>li.a".into())));
    assert_eq!(at(Syntax::Html, "  p[title='a b']", true), Some((2, "p[title='a b']".into())));
    assert_eq!(at(Syntax::Html, "<div cl", true), None, "inside a tag");
    assert_eq!(at(Syntax::Html, "<script>\nul", true), None);
    assert_eq!(at(Syntax::Html, "hello", true), None, "noise");
    assert_eq!(at(Syntax::Html, "hello", false), Some((0, "hello".into())));
    assert_eq!(at(Syntax::Html, "my-el", true), Some((0, "my-el".into())));
    assert_eq!(at(Syntax::Html, "(some text)", true), None);
    assert_eq!(at(Syntax::Html, "<style>\na {\n  m10", true), Some((2, "m10".into())));
    assert_eq!(at(Syntax::Css, "a {\n  m10", true), Some((2, "m10".into())));
    assert_eq!(at(Syntax::Css, "m10", true), None, "outside a rule");
    assert_eq!(at(Syntax::Css, "a {\n  margin: m10", true), None, "a value");
    assert_eq!(at(Syntax::Jsx, "  return (div.box", true), Some((10, "div.box".into())));
    assert_eq!(at(Syntax::Jsx, "  console.log", true), None);
    assert_eq!(at(Syntax::Jsx, "  const s = \"div", true), None);
    assert_eq!(at(Syntax::Jsx, "  <Card", true), None);
    assert_eq!(at(Syntax::Jsx, "  Card", true), Some((2, "Card".into())));
}

#[test]
fn wraps_text() {
    assert_eq!(wrap("div.w", "hello", Syntax::Html).unwrap(), "<div class=\"w\">hello</div>");
    assert_eq!(wrap("ul>li*", "a\nb", Syntax::Html).unwrap(), "<ul>\n\t<li>a</li>\n\t<li>b</li>\n</ul>");
    assert_eq!(wrap("div", "  x\n    y", Syntax::Html).unwrap(), "<div>\n\tx\n\t  y\n</div>");
}
