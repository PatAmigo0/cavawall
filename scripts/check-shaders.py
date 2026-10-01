#!/usr/bin/env python3
# Compile and link every define combination of every shader pair with
# glslangValidator. Startup builds whichever combination a config asks for,
# so a variant that fails here fails on someone's GPU at startup.
#
#   scripts/check-shaders.py
#
# The defines are read from the #ifdefs of each pair, so a new one is covered
# without touching this file
import concurrent.futures, itertools, os, re, shutil, subprocess, sys, tempfile

SRC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "shaders")
PAIRS = {
    "bars": ("vertex_shader", "fragment_shader"),
    "circle": ("circle_vertex_shader", "circle_fragment_shader"),
    "curve": ("curve_vertex_shader", "curve_fragment_shader"),
    "mask": ("mask_vertex_shader", "mask_fragment_shader"),
}

if not shutil.which("glslangValidator"):
    sys.exit("check-shaders: needs glslangValidator (the glslang package)")

def read(stem):
    with open(os.path.join(SRC, stem + ".glsl")) as f:
        return f.read()

def with_defines(text, defines):
    # After #version, which must stay first, as startup puts them
    version, rest = text.split("\n", 1)
    return version + "\n" + "".join(f"#define {d}\n" for d in defines) + rest

def check(job):
    mode, defines, out = job
    vert, frag = (read(stem) for stem in PAIRS[mode])
    base = os.path.join(out, mode + "_" + "_".join(defines))
    for ext, text in ((".vert", vert), (".frag", frag)):
        with open(base + ext, "w") as f:
            f.write(with_defines(text, defines))
    r = subprocess.run(["glslangValidator", "-l", base + ".vert", base + ".frag"], capture_output=True, text=True)
    return mode, defines, r.returncode == 0, r.stdout + r.stderr

with tempfile.TemporaryDirectory() as out:
    jobs = []
    for mode, stems in PAIRS.items():
        names = sorted({d for stem in stems for d in re.findall(r"#ifdef (\w+)", read(stem))})
        for n in range(len(names) + 1):
            jobs.extend((mode, combo, out) for combo in itertools.combinations(names, n))
    failed = 0
    with concurrent.futures.ThreadPoolExecutor(os.cpu_count() or 4) as pool:
        for mode, defines, ok, log in pool.map(check, jobs):
            if not ok:
                failed += 1
                if failed <= 3:
                    print(f"{mode} {' '.join(defines) or '(none)'}:\n{log.strip()}\n")
    print(f"{len(jobs)} variants, {failed} failed")
    sys.exit(1 if failed else 0)
