#!/usr/bin/env bash
# 等待 crate 的指定版本出现在 crates.io **sparse index**（index.crates.io）。
#
# 为什么不用 crates.io API：API 可能领先索引，API 可见并不保证下一个 crate 发布时
# 依赖解析可见（会制造 "dependency not found" 假失败）；反之 API/429 抖动又会造成
# 60s 假红。cargo publish 读取的正是 sparse index，所以按它等待。
#
# 完成语义：版本可见 ⇒ exit 0；轮询耗尽 ⇒ 显式失败（调用方依赖它继续发布，静默继续
# 会留下后续 crate 的发布缺口）。
#
# 用法：scripts/ci/wait_for_crate.sh <crate-name>
# 测试接缝：WAIT_FOR_CRATE_ATTEMPTS（默认 12）、WAIT_FOR_CRATE_INTERVAL_SECS（默认 5）。
set -euo pipefail

crate="${1:?usage: wait_for_crate.sh <crate-name>}"
attempts="${WAIT_FOR_CRATE_ATTEMPTS:-12}"
interval="${WAIT_FOR_CRATE_INTERVAL_SECS:-5}"

version=$(cargo metadata --no-deps --format-version 1 \
  | jq -r --arg name "$crate" '.packages[] | select(.name == $name) | .version')
if [[ -z "$version" || "$version" == "null" ]]; then
  echo "::error::cannot resolve version for ${crate} from cargo metadata" >&2
  exit 1
fi

# sparse index 路径规则（https://index.crates.io/）：
#   1 字符 → 1/<name>；2 字符 → 2/<name>；3 字符 → 3/<first>/<name>；
#   ≥4 字符 → <first2>/<next2>/<name>。
# 路径必须**小写**（实测混合大小写路径返回 404；cargo 解析依赖同样走小写）；
# 版本查询仍用 cargo metadata 里的原名。
crate_path=$(printf '%s' "$crate" | tr '[:upper:]' '[:lower:]')
name_len=${#crate_path}
if (( name_len == 1 )); then
  path="1/${crate_path}"
elif (( name_len == 2 )); then
  path="2/${crate_path}"
elif (( name_len == 3 )); then
  path="3/${crate_path:0:1}/${crate_path}"
else
  path="${crate_path:0:2}/${crate_path:2:2}/${crate_path}"
fi
url="https://index.crates.io/${path}"

for i in $(seq 1 "$attempts"); do
  if curl -fsS --max-time 10 --retry 3 --retry-delay 2 \
      -H "User-Agent: a2c-smcp-publish" "$url" 2>/dev/null \
    | jq -e --arg v "$version" 'select(.vers == $v)' > /dev/null 2>&1; then
    echo "${crate}@${version} is indexed"
    exit 0
  fi
  echo "Waiting for ${crate}@${version} in sparse index... (${i}/${attempts})"
  sleep "$interval"
done

echo "::error::Timed out waiting for ${crate}@${version} in the crates.io sparse index" >&2
exit 1
