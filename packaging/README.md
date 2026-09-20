# Packaging

The AUR is one git repo per package, separate from this one. This directory is
the source of truth for what goes in it; publishing is copying these two files
across and pushing.

## Once, when an AUR account exists

1. Register at https://aur.archlinux.org and add an SSH public key under
   *My Account*. The AUR authenticates by key only - there is no password push.
2. Check the name is free: https://aur.archlinux.org/packages/cavawall-git

## Publishing

```bash
git clone ssh://aur@aur.archlinux.org/cavawall-git.git
cd cavawall-git
cp ~/projects/cavawall/packaging/PKGBUILD .
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m "initial release"
git push
```

Cloning a name nobody has taken gives an empty repo and a warning, which is
how a new package is created: there is no "create package" button.

`.SRCINFO` is generated, never hand-edited, and must be regenerated and
committed in the same commit as any PKGBUILD change. The AUR rejects a push
whose `.SRCINFO` disagrees with its PKGBUILD.

## Before every push

```bash
makepkg -f                  # builds from a clean clone of the git source
namcap PKGBUILD
namcap cavawall-git-*.pkg.tar.zst
```

`namcap` on the built package is the one that matters: it reads the binary's
own `NEEDED` entries and will say so if `depends` claims too much or too
little.

## Two packages, eventually

`cavawall-git` builds from `main` and needs no release. It is what this
PKGBUILD does, and it is the right first package.

A plain `cavawall` builds from a tagged tarball, so it needs a tag first:

```bash
git tag -a v0.1.0 -m "v0.1.0" && git push origin v0.1.0
```

and then `source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")`
with a real `sha256sums`, no `pkgver()`, and no `-git` suffix, provides or
conflicts. Publishing both is normal; they conflict with each other by name.

## The one trap

This checkout sets `-C target-cpu=native` in `.cargo/config.toml`. Left alone
it leaks into the package, which then runs on the machine that built it and
dies with an illegal instruction anywhere else. `RUSTFLAGS` in the environment
takes precedence over that file, so the PKGBUILD sets it. Do not remove that
line.

It sets it with `${RUSTFLAGS:-...}`, so `makepkg.conf` wins wherever it has an
opinion. That matters: a distribution that builds on the target machine sets
`-C target-cpu=native` or a microarchitecture level there, and overriding it
would throw the distribution's own tuning away. The baseline applies only when
nothing else is configured.

### Microarchitecture levels

`x86-64` is the 2003 baseline. `-v2` adds SSE4 and POPCNT (2008 hardware),
`-v3` adds AVX2, BMI2 and FMA (2013), `-v4` adds AVX-512. A binary built for
a level refuses to start on anything below it, which is why a repository that
ships prebuilt packages to unknown machines builds the baseline, and a
distribution that builds locally, or ships a separate `-v3` repository, does
not have that constraint.

Check what a machine supports with:

```bash
/lib/ld-linux-x86-64.so.2 --help | grep x86-64-v
```

Choosing a level at BUILD time is all-or-nothing. Choosing at RUN time is a
different technique: `is_x86_feature_detected!` plus `#[target_feature]`
functions, with a dispatch decided once at startup. It costs a duplicated
implementation per level and is worth it only for a hot loop that vectorises -
which is a measurement, not an assumption.
