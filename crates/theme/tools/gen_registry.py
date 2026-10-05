#!/usr/bin/env python3
"""Generates `crates/theme/src/registry.rs` from the color registry sources.

The upstream color registry registers every theme color with a default per theme kind (dark, light, hcDark,
hcLight). Defaults are either colors or expressions over other colors (transparent(x, 0.5),
oneOf(a, b), ...), resolved against the active theme. This script parses those
`registerColor(...)` calls from TypeScript sources and emits them as a Rust table.

Usage: gen_registry.py <dir with the downloaded .ts files and extensions/git/package.json>
Sources: the MIT-licensed upstream color registry's TypeScript files (see
THIRD-PARTY-NOTICES.md), downloaded into one directory.
"""
import glob, json, os, re, sys

KINDS = ["dark", "light", "hcDark", "hcLight"]


def tokenize(s):
    toks = []
    i = 0
    while i < len(s):
        c = s[i]
        if c.isspace():
            i += 1
        elif s.startswith("//", i):
            i = s.find("\n", i) if "\n" in s[i:] else len(s)
        elif s.startswith("/*", i):
            i = s.index("*/", i) + 2
        elif c in "'\"`":
            j = i + 1
            while s[j] != c:
                j += 2 if s[j] == "\\" else 1
            toks.append(("str", s[i + 1 : j]))
            i = j + 1
        elif c.isdigit() or (c == "." and s[i + 1].isdigit()):
            m = re.match(r"[0-9.]+", s[i:])
            toks.append(("num", float(m.group())))
            i += len(m.group())
        elif c.isalpha() or c in "_$":
            m = re.match(r"[A-Za-z0-9_$.]+", s[i:])
            toks.append(("id", m.group()))
            i += len(m.group())
        else:
            toks.append(("p", c))
            i += 1
    return toks


class Parser:
    def __init__(self, toks):
        self.t = toks
        self.i = 0

    def peek(self):
        return self.t[self.i] if self.i < len(self.t) else ("eof", None)

    def next(self):
        tok = self.peek()
        self.i += 1
        return tok

    def expect(self, v):
        tok = self.next()
        assert tok[1] == v, f"expected {v}, got {tok}"

    def expr(self):
        e = self.primary()
        # Method chains like Color.fromHex('#797979').transparent(0.4).
        while self.peek() == ("p", "."):
            self.next()
            name = self.next()[1]
            self.expect("(")
            args = [e]
            while self.peek()[1] != ")":
                args.append(self.expr())
                if self.peek()[1] == ",":
                    self.next()
            self.next()
            e = ("call", name, args)
        return e

    def primary(self):
        kind, v = self.next()
        if kind == "str":
            return ("hex", v) if v.startswith("#") else ("ref", v)
        if kind == "num":
            return ("num", v)
        if kind == "p" and v == "{":
            obj = {}
            while self.peek()[1] != "}":
                key = self.next()[1]
                self.expect(":")
                obj[key] = self.expr()
                if self.peek()[1] == ",":
                    self.next()
            self.next()
            return ("obj", obj)
        if kind == "id":
            if v == "null" or v == "undefined":
                return ("null",)
            if v == "new":
                return self.expr()
            # Color.white.transparent(0.1): split the constant from the method name.
            parts = v.split(".")
            if len(parts) == 3 and parts[0] == "Color":
                self.i -= 1
                self.t[self.i] = ("id", parts[0] + "." + parts[1])
                self.t.insert(self.i + 1, ("p", "."))
                self.t.insert(self.i + 2, ("id", parts[2]))
                return self.primary()
            if v in ("Color.white", "Color.black", "Color.transparent", "Color.red", "Color.blue", "Color.green", "Color.cyan", "Color.lightgrey"):
                return ("hex", {"Color.white": "#FFFFFF", "Color.black": "#000000", "Color.transparent": "#00000000",
                                "Color.red": "#FF0000", "Color.blue": "#0000FF", "Color.green": "#00FF00",
                                "Color.cyan": "#00FFFF", "Color.lightgrey": "#D3D3D3"}[v])
            if self.peek()[1] == "(":
                self.next()
                args = []
                while self.peek()[1] != ")":
                    args.append(self.expr())
                    if self.peek()[1] == ",":
                        self.next()
                self.next()
                return ("call", v, args)
            return ("var", v)
        raise ValueError(f"unexpected {kind} {v}")


def find_calls(src):
    """Yields (const_name or None, id, defaults expr) for each registerColor call."""
    for m in re.finditer(r"(?:(?:export\s+)?const\s+([A-Za-z0-9_]+)\s*=\s*)?registerColor\(", src):
        if src[: m.start()].rstrip().endswith("function"):
            continue
        # Balanced parentheses to get the argument text.
        depth, j = 1, m.end()
        in_str = None
        while depth:
            c = src[j]
            if in_str:
                if c == "\\":
                    j += 1
                elif c == in_str:
                    in_str = None
            elif c in "'\"`":
                in_str = c
            elif c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
            j += 1
        p = Parser(tokenize(src[m.end() : j - 1]))
        first = p.expr()
        if first[0] != "ref":
            continue  # computed ids (the terminal ANSI loop) are handled separately
        p.expect(",")
        yield m.group(1), first[1], p.expr()


def main():
    root = sys.argv[1]
    defaults = {}  # id -> {kind: expr}
    var_to_id = {}
    consts = {}  # plain `const x = <color or number>;` helpers
    for path in sorted(glob.glob(os.path.join(root, "*.ts"))):
        src = open(path).read()
        for m in re.finditer(r"^(?:export\s+)?const\s+([A-Za-z0-9_]+)\s*=\s*((?:new Color|Color\.|[0-9.]+)[^;]*);", src, re.M):
            try:
                consts[m.group(1)] = Parser(tokenize(m.group(2))).expr()
            except (ValueError, AssertionError, IndexError):
                pass
        for const, cid, d in find_calls(src):
            if const:
                var_to_id[const] = cid
            if d[0] == "obj":
                defaults[cid] = {k: d[1].get(k, ("null",)) for k in KINDS}
            else:
                defaults[cid] = {k: d for k in KINDS}
        if "ansiColorMap" in src:
            body = src[src.index("export const ansiColorMap") :]
            body = body[body.index("= {") + 2 :]
            obj = Parser(tokenize(body)).expr()[1]
            for cid, entry in obj.items():
                d = entry[1]["defaults"][1]
                defaults[cid] = {k: d.get(k, ("null",)) for k in KINDS}
    pkg = os.path.join(root, "package.json")
    if os.path.exists(pkg):
        for c in json.load(open(pkg))["contributes"]["colors"]:
            d = c["defaults"]
            vals = [d.get("dark"), d.get("light"), d.get("highContrast"), d.get("highContrastLight")]
            defaults[c["id"]] = {
                k: (("null",) if v is None else ("hex", v) if v.startswith("#") else ("ref", v)) for k, v in zip(KINDS, vals)
            }

    def rust(e):
        tag = e[0]
        if tag == "null":
            return "E::Null"
        if tag == "hex":
            h = e[1].lstrip("#")
            if len(h) in (3, 4):
                h = "".join(ch * 2 for ch in h)
            if len(h) == 6:
                h += "FF"
            return f"E::Hex(0x{h.upper()})"
        if tag == "ref":
            return f'E::Ref("{e[1]}")'
        if tag == "var" and e[1] not in var_to_id and e[1] in consts:
            return rust(consts[e[1]])
        if tag == "var":
            if e[1] not in var_to_id:
                raise ValueError(f"unknown color variable {e[1]}")
            return f'E::Ref("{var_to_id[e[1]]}")'
        if tag == "call":
            name, args = e[1], e[2]
            def num(a):
                if a[0] == "var":
                    a = consts[a[1]]
                return repr(float(a[1]))
            if name in ("transparent", "darken", "lighten"):
                variant = {"transparent": "Transparent", "darken": "Darken", "lighten": "Lighten"}[name]
                return f"E::{variant}(&{rust(args[0])}, {num(args[1])})"
            if name == "opaque":
                return f"E::Opaque(&{rust(args[0])}, &{rust(args[1])})"
            if name == "oneOf":
                return "E::OneOf(&[" + ", ".join(rust(a) for a in args) + "])"
            if name == "ifDefinedThenElse":
                cid = args[0][1] if args[0][0] == "ref" else var_to_id[args[0][1]]
                return f'E::IfDefined("{cid}", &{rust(args[1])}, &{rust(args[2])})'
            if name == "lessProminent":
                return f"E::LessProminent(&{rust(args[0])}, &{rust(args[1])}, {num(args[2])}, {num(args[3])})"
            if name == "Color.fromHex":
                return rust(("hex", args[0][1]))
            if name == "Color":
                return rust(args[0])
            if name == "RGBA":
                r, g, b = (int(a[1]) for a in args[:3])
                a = args[3][1] if len(args) > 3 else 1.0
                return f"E::Hex(0x{r:02X}{g:02X}{b:02X}{round(a * 255):02X})"
            raise ValueError(f"unsupported call {name}")
        raise ValueError(f"unsupported expr {e}")

    out = [
        "// Generated by tools/gen_registry.py from MIT-licensed color registry sources (see",
        "// THIRD-PARTY-NOTICES.md). Do not edit by hand.",
        "",
        "use crate::registry_expr::Expr as E;",
        "",
        "/// (color id, defaults for dark, light, high contrast dark, high contrast light).",
        "pub static DEFAULTS: &[(&str, [E; 4])] = &[",
    ]
    skipped = []
    for cid in sorted(defaults):
        try:
            vals = [rust(defaults[cid][k]) for k in KINDS]
        except ValueError as err:
            skipped.append(f"{cid}: {err}")
            continue
        out.append(f'    ("{cid}", [{", ".join(vals)}]),')
    out.append("];")
    open(os.path.join(os.path.dirname(__file__), "..", "src", "registry.rs"), "w").write("\n".join(out) + "\n")
    print(f"{len(defaults) - len(skipped)} colors", file=sys.stderr)
    for s in skipped:
        print("skipped", s, file=sys.stderr)


main()
