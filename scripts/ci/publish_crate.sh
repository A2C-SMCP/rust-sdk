#!/usr/bin/env bash
# 发布单个 crate 到 crates.io，仅容忍「该版本已存在」类错误。
#
# 背景：publish.yml 历史上对每个 cargo publish 无条件 `|| true`，token 失效 / 依赖未索引 /
# 网络错误都会被吞掉，job 仍绿但 crate 未上线（v0.3.2 教训）。
#
# 幂等判定必须**双重确认**，否则误判会直接制造假绿：
#   1) 失败输出命中锚定的 crates.io「已上传」消息（不接受泛化的 "already exists"——
#      编译诊断/源码行/无关 warning 都可能含该短语）；
#   2) crates.io API 确认该 crate@version 确实存在（API 抖动/不存在时不放行，宁可红）。
#
# 用法：scripts/ci/publish_crate.sh <crate-name>
# 令牌：环境变量 CARGO_REGISTRY_TOKEN（cargo 原生读取，不经命令行，避免进入进程列表/日志）。
set -euo pipefail

crate="${1:?usage: publish_crate.sh <crate-name>}"
if [[ -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
  echo "::error::CARGO_REGISTRY_TOKEN is not set; refusing to publish ${crate}" >&2
  exit 1
fi

crate_version() {
  cargo metadata --no-deps --format-version 1 \
    | jq -r --arg name "$crate" '.packages[] | select(.name == $name) | .version'
}

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

set +e
cargo publish -p "$crate" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
set -e

if [[ $status -eq 0 ]]; then
  echo "${crate} published"
  exit 0
fi

already_published_message() {
  # 只接受锚定的真实消息（逐行匹配，避免泛化子串误判）：
  #   crates.io 400: crate version `1.2.3` is already uploaded
  #   index 侧旧消息: crate `foo@1.2.3` already exists on crates.io index
  #   cargo 本地检查: cannot publish over previously published version 1.2.3
  grep -Eqi 'crate version .*is already uploaded' "$log" && return 0
  grep -Eqi 'crate .*@[^ ]+ already exists on .* index' "$log" && return 0
  grep -Eqi 'cannot publish over previously published version [^ ]+' "$log" && return 0
  return 1
}

api_confirms_version() {
  local version body
  version=$(crate_version)
  if [[ -z "$version" || "$version" == "null" ]]; then
    return 1
  fi
  body=$(curl -fsS --max-time 20 --retry 3 --retry-delay 2 \
    -H "User-Agent: a2c-smcp-publish" \
    "https://crates.io/api/v1/crates/${crate}/${version}" 2>/dev/null) || return 1
  jq -e --arg v "$version" '.version.num == $v' <<<"$body" > /dev/null 2>&1
}

if already_published_message && api_confirms_version; then
  version=$(crate_version)
  echo "::notice::${crate}@${version} 已存在于 crates.io（重跑发布），跳过"
  exit 0
fi

echo "::error::cargo publish -p ${crate} 失败（非已确认的 already-uploaded），发布中止" >&2
exit "$status"
