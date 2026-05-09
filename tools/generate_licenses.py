#!/usr/bin/env python3
"""Generate THIRD_PARTY_LICENSES.md for a Cargo binary crate.

Usage:
  python3 tools/generate_licenses.py cdfuse/Cargo.toml THIRD_PARTY_LICENSES.md
"""
import json, subprocess, sys, pathlib

# Crates that are part of this project -- exclude from third-party list.
OWN_CRATES = {"cdcore", "cdfuse", "cdwinfs", "ddsthumb"}

# Standard SPDX license texts for crates that omit a LICENSE file.
SPDX_TEXTS = {
    "MIT": """\
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.""",

    "Apache-2.0": "See https://www.apache.org/licenses/LICENSE-2.0",

    "BSD-3-Clause": """\
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.""",

    "Zlib": """\
This software is provided 'as-is', without any express or implied warranty.
In no event will the authors be held liable for any damages arising from the
use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software in a
   product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution.""",
}

def spdx_fallback(license_expr):
    """Return a standard text if the expression is a single known SPDX id."""
    for key in SPDX_TEXTS:
        if license_expr == key:
            return SPDX_TEXTS[key]
    # OR expressions: pick the first recognisable option
    for part in license_expr.replace("(", "").replace(")", "").split(" OR "):
        part = part.strip()
        if part in SPDX_TEXTS:
            return f"(Using {part} option)\n\n" + SPDX_TEXTS[part]
    return None

def find_license_text(name, version):
    registry = pathlib.Path.home() / ".cargo/registry/src"
    for idx in registry.iterdir():
        pkg = idx / f"{name}-{version}"
        if not pkg.exists():
            continue
        for candidate in ["LICENSE", "LICENSE.md", "LICENSE-MIT", "LICENSE-APACHE",
                           "LICENSE-BSD", "LICENCE", "COPYING"]:
            p = pkg / candidate
            if p.exists():
                return p.read_text(errors="replace").strip()
        files = (sorted(pkg.glob("LICENSE*")) + sorted(pkg.glob("LICENCE*"))
                 + sorted(pkg.glob("COPYING*")))
        if files:
            parts = [f"--- {f.name} ---\n" + f.read_text(errors="replace").strip() for f in files]
            return "\n\n".join(parts)
    return None

def main():
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <manifest_path> <output_file>", file=sys.stderr)
        sys.exit(1)
    manifest, outfile = sys.argv[1], sys.argv[2]

    result = subprocess.run(
        ["cargo", "license", "--manifest-path", manifest, "--json"],
        capture_output=True, text=True
    )
    if result.returncode != 0:
        print("cargo license failed:", result.stderr, file=sys.stderr)
        sys.exit(1)

    all_deps = json.loads(result.stdout)
    seen = {}
    for d in all_deps:
        key = (d["name"], d["version"])
        if key not in seen and d["name"] not in OWN_CRATES:
            seen[key] = d
    deps = sorted(seen.values(), key=lambda d: d["name"].lower())

    lines = []
    lines.append("# Third-Party Licenses\n")
    binary = pathlib.Path(manifest).parent.name  # e.g. "cdfuse" or "cdwinfs"
    lines.append(f"This document lists all third-party dependencies included in the {binary} binary")
    lines.append("and their licenses. Generated by `cargo-license`.\n")

    lines.append("## Summary\n")
    lines.append("| Package | Version | License |")
    lines.append("|---------|---------|---------|")
    for d in deps:
        lic  = d.get("license") or "UNKNOWN"
        repo = d.get("repository") or ""
        cell = f"[{d['name']}]({repo})" if repo else d["name"]
        lines.append(f"| {cell} | {d['version']} | {lic} |")
    lines.append("")

    lines.append("## License Texts\n")
    missing = []
    for d in deps:
        name = d["name"]
        ver  = d["version"]
        lic  = d.get("license") or "UNKNOWN"
        text = find_license_text(name, ver) or spdx_fallback(lic)
        lines.append(f"### {name} {ver}\n")
        lines.append(f"**License:** {lic}  ")
        if d.get("repository"):
            lines.append(f"**Repository:** {d['repository']}  ")
        lines.append("")
        if text:
            lines.append("```")
            lines.append(text)
            lines.append("```")
        else:
            lines.append(f"*License text unavailable locally. SPDX: `{lic}`*")
            missing.append(f"{name} {ver} ({lic})")
        lines.append("")

    with open(outfile, "w") as f:
        f.write("\n".join(lines) + "\n")

    print(f"Written {outfile} ({len(deps)} third-party packages)")
    if missing:
        print(f"No text found (needs manual addition): {', '.join(missing)}", file=sys.stderr)

if __name__ == "__main__":
    main()
