# Releasing

How a new version gets from a commit to `apt upgrade`. Packages are built
locally in the build container; CI only assembles and signs the APT repository.

## 1. Version and checks

In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add
a new empty `## [Unreleased]` above it and update the links at the bottom.
That section becomes the release notes (`NOTES.md` below).

```sh
# bump version in Cargo.toml, then:
podman build -t fgw-build contrib/build
podman run --rm -v .:/src:Z fgw-build sh -c 'cargo update -p firegeowall --offline && cargo fmt --check && cargo clippy --release --locked -- -D warnings && cargo test --release --locked'
podman run --rm --cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN \
    -v .:/src:Z fgw-build sh -c 'cargo build --locked && sh tests/netns.sh'
git commit -am "Release vX.Y.Z" && git push
git tag -a vX.Y.Z -m "fgw vX.Y.Z" && git push origin vX.Y.Z
```

## 2. Build the assets from the tag

From a clean export, never from the working tree:

```sh
V=X.Y.Z; W=$(mktemp -d)
git archive v$V | tar -x -C $W/ && cd $W
podman run --rm -v .:/src:Z fgw-build sh -c 'cargo build --release --locked && cargo deb --no-build --no-strip'
N=fgw-v$V-x86_64-unknown-linux-musl
mkdir $N && cp target/release/fgw README.md LICENSE-* $N/ && cp -r contrib docs $N/
tar --owner=0 --group=0 -czf $N.tar.gz $N
cp target/debian/firegeowall_$V-1_amd64.deb .
sha256sum $N.tar.gz > $N.tar.gz.sha256
sha256sum firegeowall_$V-1_amd64.deb > firegeowall_$V-1_amd64.deb.sha256
```

## 3. Publish

```sh
# release notes = the version's section of CHANGELOG.md, without its heading
awk -v v="## [$V]" 'index($0,v)==1{f=1;next} /^## \[/{f=0} f' CHANGELOG.md > NOTES.md
gh release create v$V --verify-tag --title "fgw v$V" --notes-file NOTES.md \
    firegeowall_$V-1_amd64.deb firegeowall_$V-1_amd64.deb.sha256 \
    $N.tar.gz $N.tar.gz.sha256
```

Publishing the release triggers [`.github/workflows/apt-repo.yml`](../.github/workflows/apt-repo.yml):
it downloads the `.deb` of every published release, checks each against its
`.sha256`, builds the index with `apt-ftparchive`, signs `InRelease` and
`Release.gpg` and deploys to GitHub Pages. Every `.deb` needs its `.sha256`
next to it, otherwise the workflow stops.

Manual re-run (for example after changing the workflow):
`gh workflow run apt-repo -R TZERO78/firegeowall`.

## Repository key

- Ed25519, fingerprint `CF41 7369 2FFE BB3F 8BFA  9BD8 41E5 BAAD 4FCB A55B`,
  expires **2029-11-02**.
- Private key: GitHub Actions secret `APT_SIGNING_KEY` plus an offline backup
  with the revocation certificate. The workflow refuses any other key.
- Public key: [`contrib/apt/`](../contrib/apt/) and on the Pages site.
- `scripts/install.sh` pins the SHA-256 of `firegeowall-archive-keyring.gpg`.

Extending the expiry (same key, so users change nothing): import the private
key, `gpg --quick-set-expire CF4173692FFEBB3F8BFA9BD841E5BAAD4FCBA55B 3y`,
update the secret, re-export the public key into `contrib/apt/` (both `.asc` and
`.gpg`) and update `KEY_SHA256` in `scripts/install.sh`. Users who installed
the old public key must fetch the new one before it expires (re-running
`install.sh` does that).
