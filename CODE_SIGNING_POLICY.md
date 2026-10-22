# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by
[SignPath Foundation](https://signpath.org).

## What is signed

The Windows executables published on the
[releases page](https://github.com/santa326/steptrim/releases) of this repository:
`steptrim.exe`, `stepinfo.exe`, `stepprune.exe`, `stepreplicate.exe`, and the installer
`StepTrim-Setup-<version>.exe`. Every signed binary carries the product name "StepTrim" and the
release version in its file properties.

## How releases are built

Every release is built from a tagged commit of this repository by GitHub Actions on
GitHub-hosted runners, with the workflow in
[`.github/workflows/release.yml`](.github/workflows/release.yml). The workflow uploads the
unsigned executables as a workflow artifact, submits them to SignPath, and only then builds the
installer from the signed executables and submits that too. SignPath verifies that each artifact
was produced by that workflow from this repository before signing it. No binary is built or
signed on a developer's machine.

## Team and roles

| Role | Who |
| --- | --- |
| Authors (committers) | [Ravi Teja Potineni (santa326)](https://github.com/santa326) |
| Reviewers | Every change from outside the team is reviewed by the author above before it is merged |
| Approvers (signing requests) | [Ravi Teja Potineni (santa326)](https://github.com/santa326) |

All team members use multi-factor authentication for GitHub and SignPath. Changes to the CI
configuration, the installer script and the build scripts are reviewed like any other code.

## Privacy policy

This program will not transfer any information to other networked systems unless specifically
requested by the user or the person installing or operating it.

StepTrim reads and writes only the files the user opens or saves. It makes no network
connections. The optional Full load feature starts a locally installed copy of FreeCAD on the
user's own machine and exchanges temporary files with it in the user's temporary folder; nothing
leaves the machine. The installer offers an uninstaller and removes everything it installed.

## Licence

StepTrim is released under the [MIT License](LICENSE). It links only open-source Rust crates
under MIT, Apache-2.0, ISC, Zlib or Unlicense terms, listed in the README's credits.
