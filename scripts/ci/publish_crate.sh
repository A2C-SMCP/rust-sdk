#!/usr/bin/env bash
# 发布单个 crate 到 crates.io，仅容忍「该版本已存在」类错误。
#
# 背景：publish.yml 历史上对每个 cargo publish 无条件 `|| true`，token 失效 / 依赖未索引 /
# 网络错误都会被吞掉，job 仍绿但 crate 未上线（v0.3.2 教训）。本脚本只把 crates.io 的
# 「版本已上传」当幂等成功，其余失败一律原样上抛给 job。
#
# 用法：scripts/ci/publish_crate.sh <crate-name>
# 令牌：环境变量 CARGO_REGISTRY_TOKEN（cargo 原生读取，不经命令行，避免进入进程列表/日志）。
set -euo pipefail

crate="${1:?usage: publish_crate.sh <crate-name>}"
if [[ -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
  echo "::error::CARGO_REGISTRY_TOKEN is not set; refusing to publish ${crate}" >&2
  exit 1
fi

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

# 重跑发布（release 重触发 / 部分 crate 已上传）时 crates.io 返回 400；只有这一类错误放行。
if grep -Eqi 'is already uploaded|already exists|previously published version|already been published' "$log"; then
  echo "::notice::${crate} 的该版本已存在于 crates.io（重跑发布），跳过"
  exit 0
fi

echo "::error::cargo publish -p ${crate} 失败（非 already-exists），发布中止" >&2
exit "$status"
