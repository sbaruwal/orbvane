#!/usr/bin/env python3
"""Regenerates crates/css/data/css-data.json and crates/html/data/html-data.json from an
MIT-licensed web custom data package (browser data, with descriptions from MDN Web Docs under
CC-BY-SA 2.5; see THIRD-PARTY-NOTICES.md), keeping only the fields the CSS and HTML language
services use.

Usage: update_data.py <path to the package's data/ directory>
"""
import json
import os
import sys

src = sys.argv[1]
root = os.path.join(os.path.dirname(__file__), "..", "..")


def text(d):
    if isinstance(d, dict):
        return d.get("value", "")
    return d or ""


def mdn(entry):
    for r in entry.get("references", []):
        if r.get("name") == "MDN Reference":
            return r["url"]
    return None


def entry(e, extra=()):
    out = {"name": e["name"]}
    if text(e.get("description")):
        out["description"] = text(e["description"])
    for key in ("browsers", "syntax", "relevance", "status", "restrictions", *extra):
        if key in e:
            out[key] = e[key]
    if "baseline" in e:
        b = e["baseline"]
        out["baseline"] = {k: b[k] for k in ("status", "baseline_low_date") if k in b}
    if mdn(e):
        out["mdn"] = mdn(e)
    return out


def values(vs):
    return [{k: v for k, v in (("name", x["name"]), ("description", text(x.get("description")))) if v} for x in vs]


css = json.load(open(os.path.join(src, "browsers.css-data.json")))
out = {}
for kind in ("properties", "atDirectives", "pseudoClasses", "pseudoElements"):
    items = []
    for e in css[kind]:
        x = entry(e)
        if e.get("values"):
            x["values"] = values(e["values"])
        items.append(x)
    out[kind] = items
with open(os.path.join(root, "css", "data", "css-data.json"), "w") as f:
    json.dump(out, f, separators=(",", ":"), ensure_ascii=False)

html = json.load(open(os.path.join(src, "browsers.html-data.json")))


def attribute(a):
    x = entry(a, ("valueSet",))
    if a.get("values"):
        x["values"] = values(a["values"])
    return x


out = {
    "tags": [dict(entry(t, ("void",)), attributes=[attribute(a) for a in t.get("attributes", [])]) for t in html["tags"]],
    "globalAttributes": [attribute(a) for a in html["globalAttributes"]],
    "valueSets": [{"name": v["name"], "values": values(v["values"])} for v in html["valueSets"]],
}
with open(os.path.join(root, "html", "data", "html-data.json"), "w") as f:
    json.dump(out, f, separators=(",", ":"), ensure_ascii=False)
