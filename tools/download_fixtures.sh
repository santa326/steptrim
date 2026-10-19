#!/usr/bin/env bash
# Downloads the public STEP sample assemblies from steptools.com into fixtures/steptools.
# Re-runnable: skips files that already exist with a non-zero size.
set -u
DEST="$(dirname "$0")/../fixtures/steptools"
mkdir -p "$DEST"
BASE="https://www.steptools.com/docs/stpfiles"
FILES=(
  "bigassy/Scania-Engine-V8-XT-Turbo.step"
  "bigassy/Scania-8x4.stp"
  "bigassy/NissanGT-R.STEP"
  "bigassy/Ai-14R.stp"
  "bigassy/UMC-500_SS_Solid_Model_2019-06_r1.stp"
  "bigassy/Cruise_Assembly.stp"
  "bigassy/Rocky_House.stp"
  "bigassy/ROTOR-201NAL-Z7.STEP"
  "ap203/rear.stp"
  "ap203/st203-bapl.stp"
  "ap214/as1-ec-214.stp"
  "ap214/as1-tc-214.stp"
)
for f in "${FILES[@]}"; do
  name="$(basename "$f")"
  out="$DEST/$name"
  if [ -s "$out" ]; then echo "skip $name (exists)"; continue; fi
  echo "get  $name"
  curl -L --fail --retry 3 --retry-delay 5 -o "$out.part" "$BASE/$f" && mv "$out.part" "$out" || { echo "FAIL $name"; rm -f "$out.part"; }
done
echo "--- done ---"
ls -la "$DEST"
