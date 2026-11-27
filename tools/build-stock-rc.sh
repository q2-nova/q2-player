#!/usr/bin/env bash
set -euo pipefail

inputs="$PWD/build/stock-rc-inputs"
os="$inputs/q2-os"
image="$os/build/stock-rc-image"
package=build/stock-rc-package
boot=build/stock-rc-boot
release=build/stock-rc-release
versions=$(python3 -c '
import runpy
from pathlib import Path
print(*runpy.run_path("tools/stock-draft.py")["metadata"](Path("stock-release.json")))
')
read -r version starting_version <<< "$versions"

python3 "$os/os/compose_stock.py" prepare --assets "$inputs/q2-os-assets" \
  --output "$os/build/stock-rc-inputs"
python3 "$os/os/compose_stock.py" image --assets "$inputs/q2-os-assets" \
  --stock-inputs "$os/build/stock-rc-inputs" --player-source "$PWD" \
  --app "$PWD/build/paired-player/q2nd" --tsrsd "$inputs/tsrs" \
  --tsrs-source "$inputs/tsrs-source" --release-tag "$version" \
  --qemu-user "$inputs/qemu/bin/qemu-mipsel" --output "$image"
jq -e --arg sha "$(jq -r .artifacts.q2nd.sha256 build/paired-player/result.json)" \
  '.startup_read_ahead.app_sha256 == $sha' "$image/artifact.json" > /dev/null
python3 -c 'import runpy, sys; runpy.run_path("tools/stock-draft.py")["check_bluetooth"](sys.argv[1])' \
  "$image/rootfs.squashfs"
mkdir "$release"
python3 "$os/os/export_stock_radio.py" --stock-inputs "$os/build/stock-rc-inputs" \
  --image "$image" --sdk "$inputs/sdk/mipsel-buildroot-linux-gnu_sdk-buildroot" \
  --output "$release/stock-radio-source.tar.gz"
python3 tools/verify-package.py pack-stock --os-repo "$os" \
  --component-lock "$inputs/component-lock.json" --image "$image" \
  --version "$version" --installed-version "$starting_version" --out "$package"
python3 tools/verify-package.py boot-stock --os-repo "$os" \
  --component-lock "$inputs/component-lock.json" --package "$package/update.tar" \
  --pack-receipt "$package/result.json" --qemu "$inputs/qemu/bin/qemu-system-mipsel" \
  --qemu-data "$inputs/qemu/share/qemu" --native-qemu --out "$boot"

cp "$package/update.tar" "$release/update.tar"
jq -s --arg version "$version" --arg starting "$starting_version" \
  '.[0] + {version: $version, starting_version: $starting, player_commit: env.STOCK_PLAYER_COMMIT, player_tag: env.GITHUB_REF_NAME,
           components: .[1], release_inputs: .[2], stock_pack: .[3]}' \
  "$boot/result.json" "$inputs/component-lock.json" "$inputs/producer-inputs.json" \
  "$package/result.json" > "$release/result.json"
python3 tools/release-source.py --package "$release/update.tar" \
  --record "$release/result.json" --producer-inputs "$inputs/producer-inputs.json" \
  --out "$release/player-source.tar.gz"
python3 tools/release-symbols.py pack --build build/paired-player \
  --out "$release/q2nd-symbols.tar.gz" > /dev/null
(cd "$release" && sha256sum update.tar result.json player-source.tar.gz q2nd-symbols.tar.gz stock-radio-source.tar.gz > SHA256SUMS)
