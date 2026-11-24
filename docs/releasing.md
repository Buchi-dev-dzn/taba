# Releasing taba

How taba versions are chosen and how a release is cut. These rules apply to
every release, patch releases included.

## Versioning rules

1. **Patch by default.** While taba is 0.x, a release is the next 0.2.x unless
   rule 2 applies. Bug fixes, platform fixes, packaging, docs, performance work
   and backward-compatible additions (a new command or flag, a new field in
   `--json` output) all ship as the next patch: 0.2.1, 0.2.2, and so on. This
   matches Cargo's convention for 0.x versions, where 0.2.0 → 0.2.1 is a
   compatible upgrade.
2. **Minor bump (0.3.0) only for a breaking change.** A change is breaking if
   it:
   - removes or renames a command or flag;
   - changes the meaning of an exit code;
   - removes a field from `--json` output or changes a field's type or meaning;
   - introduces a state or config format that older taba cannot read, or that
     needs a migration.

   A minor release is planned as a milestone in [plan.md](plan.md). Its
   release notes list every breaking change together with upgrade steps. The
   owner decides each minor bump before it is tagged.
3. **Release small and often.** Once a user-visible fix or addition is on
   `main` and CI is green, cut a patch release instead of batching many
   changes toward a bigger version.
4. **One process for every release.** See [Release steps](#release-steps).
   Tags are never moved or reused. A bad release is fixed by the next patch,
   not by replacing assets or re-pointing a tag.
5. **When unsure, keep it compatible.** If it is unclear whether a change is
   breaking, treat the breaking form as not shipping yet: make the change
   compatible (for example, keep the old flag or field as an alias) and ship
   it as a patch.

## Publication channels

GitHub Releases is the only publication channel. Publishing to package
registries (crates.io, Homebrew, winget, Scoop) and announcing a release
anywhere require the owner's approval first.

## Release steps

1. **Release PR.** Bump `version` in `Cargo.toml` (and the matching entry in
   `Cargo.lock`), and add release notes as `docs/releases/vX.Y.Z.md`. Update
   version-specific text in the README, such as the archive names and the
   pinned `cargo install --tag` command under Install. Merge once CI is green.
2. **Build the candidate from the exact `main` commit.** The
   [Release candidate](../.github/workflows/candidate.yml) workflow runs on
   every push to `main` (and on pull requests that touch packaging, `Cargo.*`
   or `src/`; those artifacts only exercise packaging and are never release
   assets). To rerun it while the release commit is still the tip of `main`:

   ```sh
   gh workflow run candidate.yml --ref main -f expected_commit=FULL_COMMIT_SHA
   ```

   The dispatched run fails unless `expected_commit` equals the checked-out
   commit. For each target (macOS arm64 and x86_64, Windows x86_64, Linux
   x86_64 and arm64, each built natively), the workflow checks that the
   toolchain host matches the target, runs `cargo test --locked --release`
   and `cargo build --locked --release`, then runs
   `scripts/package-release.py package`. That step writes
   `taba-vX.Y.Z-TARGET.tar.gz` (`.zip` on Windows) containing the binary,
   `LICENSE` and `README.md`, re-reads the archive to check its exact contents
   and bytes, runs the extracted binary to check that `taba --version` prints
   the `Cargo.toml` version, and writes a `.sha256` sidecar.

   The `assemble` job then runs `scripts/package-release.py assemble`, which
   requires exactly the five expected archives and their sidecars, checks each
   sidecar against the archive, and writes `SHA256SUMS` and `COMMIT`. The job
   finishes with `sha256sum --check SHA256SUMS` and uploads everything as the
   `taba-candidate-<commit>` artifact. The workflow only has read access to
   the repository and uploads Actions artifacts; it never publishes.
3. **Verify the candidate.** Download `taba-candidate-<commit>`. Check that
   `COMMIT` is the intended release commit on `main`, that the archive names
   carry the new version, and that every archive matches `SHA256SUMS`
   (`sha256sum --check SHA256SUMS`, or `shasum -a 256 -c SHA256SUMS` on
   macOS).
4. **Tag and publish.** Tag that commit `vX.Y.Z` and create the GitHub
   Release from `docs/releases/vX.Y.Z.md`, attaching the verified archives and
   `SHA256SUMS` from the candidate artifact unchanged. This is an
   owner-controlled step.
