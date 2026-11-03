#!/usr/bin/env bash
# 发布单个 crate 到 crates.io，仅容忍「该版本已存在且内容一致」类错误。
#
# 背景：publish.yml 历史上对每个 cargo publish 无条件 `|| true`，token 失效 / 依赖未索引 /
# 网络错误都会被吞掉，job 仍绿但 crate 未上线（v0.3.2 教训）。
#
# 幂等判定必须**双重确认**，否则误判会直接制造假绿：
#   1) 失败输出命中锚定的 crates.io「已上传」消息（不接受泛化的 "already exists"——
#      编译诊断/源码行/无关 warning 都可能含该短语）；
#   2) crates.io API 确认该 crate@version 存在、未被 yank，且 **checksum 与本地
#      `cargo package` 产物一致**——防止「从不同 commit 重跑同版本号」造成混合来源：
#      smcp 被跳过、下游却从新 commit 发布。
#
# 用法：scripts/ci/publish_crate.sh <crate-name>
# 令牌：环境变量 CARGO_REGISTRY_TOKEN（cargo 原生读取，不经命令行，避免进入进程列表/日志）。
set -euo pipefail

crate="${1:?usage: publish_crate.sh <crate-name>}"
if [[ -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
  echo "::error::CARGO_REGISTRY_TOKEN is not set; refusing to publish ${crate}" >&2
  exit 1
fi

sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

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
  #   本地 index 重复: crate `foo@1.2.3` already exists on crates.io index
  grep -Eqi 'crate version .*is already uploaded' "$log" && return 0
  grep -Eqi 'crate .*@[^ ]+ already exists on .* index' "$log" && return 0
  return 1
}

# 已存在版本的内容校验：本地 .crate 的 sha256 必须与 crates.io 记录一致，且版本未 yank。
verify_already_published() {
  local version target_dir crate_file local_sum body yanked remote_sum
  version=$(crate_version)
  if [[ -z "$version" || "$version" == "null" ]]; then
    echo "::error::cannot resolve version for ${crate} from cargo metadata" >&2
    return 1
  fi
  target_dir=$(cargo metadata --no-deps --format-version 1 \
    | jq -r '.target_directory') || return 1
  # 失败发生在 upload 前/后都可能留下旧的 .crate；显式重新打包保证比对的是当前源码。
  if ! cargo package -p "$crate" --no-verify > /dev/null 2>&1; then
    echo "::error::cannot build local .crate for ${crate}@${version}; cannot verify already-published version" >&2
    return 1
  fi
  crate_file="${target_dir}/package/${crate}-${version}.crate"
  if [[ ! -f "$crate_file" ]]; then
    echo "::error::local .crate not found at ${crate_file}; cannot verify already-published version" >&2
    return 1
  fi
  local_sum=$(sha256_file "$crate_file")

  body=$(curl -fsS --max-time 20 --retry 3 --retry-delay 2 \
    -H "User-Agent: a2c-smcp-publish" \
    "https://crates.io/api/v1/crates/${crate}/${version}" 2>/dev/null) || {
    echo "::error::crates.io API query failed; cannot verify ${crate}@${version}" >&2
    return 1
  }
  if [[ "$(jq -r '.version.num // empty' <<< "$body")" != "$version" ]]; then
    echo "::error::crates.io API does not report ${crate}@${version}; cannot treat the failure as already-published" >&2
    return 1
  fi
  yanked=$(jq -r '.version.yanked' <<< "$body")
  if [[ "$yanked" != "false" ]]; then
    echo "::error::${crate}@${version} is yanked on crates.io; refusing to treat it as published — bump the version" >&2
    return 1
  fi
  remote_sum=$(jq -r '.version.checksum // empty' <<< "$body")
  if [[ -z "$remote_sum" || "$local_sum" != "$remote_sum" ]]; then
    echo "::error::${crate}@${version} exists but checksum differs (local ${local_sum} / remote ${remote_sum:-<missing>}); the version was likely published from a different commit — bump the version" >&2
    return 1
  fi
  echo "::notice::${crate}@${version} already exists with matching checksum (rerun), skipping"
  return 0
}

if already_published_message; then
  if verify_already_published; then
    exit 0
  fi
  exit 1
fi

echo "::error::cargo publish -p ${crate} failed (not a verified already-uploaded version); aborting" >&2
exit "$status"
