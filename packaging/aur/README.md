# AUR packaging — `rumble-damascene`

Source-build AUR package for the Rumble desktop client. It compiles
`rumble-damascene` from the tagged release tarball and installs the binary plus
the `com.rumble.Rumble.desktop` launcher entry.

Files here are the canonical copy kept in-tree. The AUR itself is a separate git
repo (`ssh://aur@aur.archlinux.org/rumble-damascene.git`) that contains only the
`PKGBUILD` and `.SRCINFO`; the `.desktop` file is pulled from the release tarball
at build time, not duplicated into the AUR repo.

## Publishing a new version

1. **Cut the release first.** The `PKGBUILD` `source=` points at
   `…/archive/refs/tags/v$pkgver.tar.gz`, so the git tag (e.g. `v0.3.0`) must
   already exist on GitHub. See the repo root for the tag/push flow.

2. **Bump and checksum.** Set `pkgver` (and reset `pkgrel=1`) in `PKGBUILD`,
   then fill in the real tarball digest — `SKIP` is only a placeholder:

   ```bash
   cd packaging/aur
   updpkgsums            # from pacman-contrib; rewrites sha256sums=()
   makepkg --printsrcinfo > .SRCINFO
   ```

3. **Test the build** in a clean chroot (catches missing deps):

   ```bash
   makepkg -f                       # quick local build
   # or, hermetic:
   extra-x86_64-build               # from devtools
   namcap PKGBUILD *.pkg.tar.zst    # lint
   ```

4. **Push to the AUR** (copy `PKGBUILD` + `.SRCINFO` into the AUR clone):

   ```bash
   git clone ssh://aur@aur.archlinux.org/rumble-damascene.git aur-rumble
   cp PKGBUILD .SRCINFO aur-rumble/
   cd aur-rumble && git commit -am "rumble-damascene 0.3.0-1" && git push
   ```

## Notes

- **`options=('!lto')`** — release builds already link a lot of native
  audio/video libs; distro-wide LTO injection has been a source of breakage, so
  it is disabled here. Drop it if you confirm LTO builds cleanly.
- **Vulkan ICD** — `vulkan-icd-loader` is a hard dep, but the actual driver
  (`vulkan-radeon` / `nvidia-utils` / `vulkan-intel`) depends on the user's GPU
  and is intentionally not pinned.
- **License** — MIT (root `LICENSE`); the package installs it to
  `/usr/share/licenses/$pkgname/`.
