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

This checkout sets `-C target-cpu=native` in `.cargo/config.toml`. A package
built that way runs on the machine that built it and crashes with an illegal
instruction on anyone else's. The PKGBUILD exports `RUSTFLAGS`, which takes
precedence over that file, and appends `-C target-cpu=x86-64` so a later flag
wins over an earlier one. Do not remove it.
