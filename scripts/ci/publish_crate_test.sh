#!/usr/bin/env bash
# publish_crate.sh / wait_for_crate.sh 的假 cargo/curl 场景测试（CI validate 与本地共用）。
#
# 重点回归（架构师审查 🔴）：日志里出现泛化的 "already exists"（warning / 源码诊断行）但
# 真实失败是鉴权时，publish_crate.sh 必须非 0——历史上子串白名单会把这类失败误判为幂等。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"

cat > "$tmp/bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
case "${1:-}" in
  metadata)
    cat <<'JSON'
{"packages":[
  {"name":"fake-crate","version":"1.2.3"},
  {"name":"smcp","version":"1.2.3"},
  {"name":"a2c-smcp","version":"1.2.3"},
  {"name":"abc","version":"1.2.3"},
  {"name":"xy","version":"1.2.3"}
]}
JSON
    exit 0 ;;
  publish)
    case "${FAKE_CARGO_MODE:?FAKE_CARGO_MODE must be set}" in
      ok)
        echo "Uploading fake-crate v1.2.3" ;;
      already-uploaded)
        echo "error: failed to publish to registry at https://crates.io" >&2
        echo "Caused by: the remote server responded with an error (status 400 Bad Request): crate version \`1.2.3\` is already uploaded" >&2
        exit 101 ;;
      noisy-already-exists)
        # 真实形态：泛化短语来自 warning / 源码诊断，真实错误是鉴权失败。
        echo 'warning: unused `Result`: marketplace name conflict: "x" already exists' >&2
        echo "warning: MCP Manager already exists but process lifecycle is not running" >&2
        echo "error: failed to authenticate token" >&2
        exit 101 ;;
      auth)
        echo "error: failed to authenticate token" >&2
        exit 101 ;;
      dep)
        echo "error: dependency \`foo\` not found in registry" >&2
        exit 101 ;;
      *)
        echo "unknown FAKE_CARGO_MODE=$FAKE_CARGO_MODE" >&2
        exit 2 ;;
    esac ;;
esac
FAKE_CARGO
chmod +x "$tmp/bin/cargo"

cat > "$tmp/bin/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
if [[ -n "${FAKE_CURL_ARGS_FILE:-}" ]]; then
  printf '%s\n' "$*" >> "$FAKE_CURL_ARGS_FILE"
fi
case "$*" in
  *index.crates.io*)
    case "${FAKE_INDEX_MODE:-exists}" in
      exists) echo '{"name":"fake-crate","vers":"1.2.3"}' ;;
      missing) echo '{"name":"fake-crate","vers":"9.9.9"}' ;;
      error) exit 22 ;;
    esac ;;
  *)
    case "${FAKE_API_MODE:-exists}" in
      exists) echo '{"version":{"num":"1.2.3"}}' ;;
      missing) echo '{}' ;;
      error) exit 22 ;;
    esac ;;
esac
FAKE_CURL
chmod +x "$tmp/bin/curl"

failures=0

check() { # name expected actual
  local name=$1 expected=$2 actual=$3
  if (( actual == expected )); then
    echo "PASS ${name} (rc=${actual})"
  else
    echo "FAIL ${name}: expected rc=${expected}, got rc=${actual}"
    failures=$((failures + 1))
  fi
}

publish_case() { # name expected_rc cargo_mode api_mode token
  local name=$1 expected=$2 cargo_mode=$3 api_mode=$4 token=$5 rc
  set +e
  PATH="$tmp/bin:$PATH" CARGO_REGISTRY_TOKEN="$token" \
    FAKE_CARGO_MODE="$cargo_mode" FAKE_API_MODE="$api_mode" \
    bash "$here/publish_crate.sh" fake-crate > "$tmp/${name}.log" 2>&1
  rc=$?
  set -e
  check "$name" "$expected" "$rc"
  if (( rc != expected )); then
    sed 's/^/  | /' "$tmp/${name}.log"
  fi
}

wait_case() { # name expected_rc index_mode attempts interval crate
  local name=$1 expected=$2 index_mode=$3 attempts=$4 interval=$5 crate=$6 rc
  set +e
  PATH="$tmp/bin:$PATH" FAKE_INDEX_MODE="$index_mode" \
    WAIT_FOR_CRATE_ATTEMPTS="$attempts" WAIT_FOR_CRATE_INTERVAL_SECS="$interval" \
    bash "$here/wait_for_crate.sh" "$crate" > "$tmp/${name}.log" 2>&1
  rc=$?
  set -e
  check "$name" "$expected" "$rc"
  if (( rc != expected )); then
    sed 's/^/  | /' "$tmp/${name}.log"
  fi
}

expect_sparse_path() { # crate expected_path_fragment
  local crate=$1 expected=$2 args_file="$tmp/curl-args-${1}.txt"
  : > "$args_file"
  PATH="$tmp/bin:$PATH" FAKE_INDEX_MODE=exists FAKE_CURL_ARGS_FILE="$args_file" \
    WAIT_FOR_CRATE_ATTEMPTS=1 WAIT_FOR_CRATE_INTERVAL_SECS=0 \
    bash "$here/wait_for_crate.sh" "$crate" > /dev/null 2>&1 || true
  if grep -q "$expected" "$args_file"; then
    echo "PASS sparse-path ${crate}"
  else
    echo "FAIL sparse-path ${crate}: expected '${expected}', got: $(tr '\n' ' ' < "$args_file")"
    failures=$((failures + 1))
  fi
}

# publish_crate.sh 场景
publish_case publish-ok 0 ok exists token
publish_case already-uploaded-verified 0 already-uploaded exists token
publish_case already-uploaded-api-missing 101 already-uploaded missing token
publish_case already-uploaded-api-error 101 already-uploaded error token
publish_case noisy-already-exists-real-failure 101 noisy-already-exists exists token
publish_case auth-failure 101 auth exists token
publish_case dep-failure 101 dep exists token
publish_case missing-token 1 ok exists ""

# wait_for_crate.sh 场景（attempts/interval 用测试接缝压到 0 秒）
wait_case wait-indexed 0 exists 3 0 fake-crate
wait_case wait-timeout 1 missing 2 0 fake-crate
wait_case wait-index-http-error 1 error 2 0 fake-crate

# sparse index 路径规则：≥4 字符 <first2>/<next2>/<name>，3 字符 3/<c>/<name>，2 字符 2/<name>
expect_sparse_path a2c-smcp "index.crates.io/a2/c-/a2c-smcp"
expect_sparse_path abc "index.crates.io/3/a/abc"
expect_sparse_path xy "index.crates.io/2/xy"

if (( failures > 0 )); then
  echo "::error::${failures} 个用例失败" >&2
  exit 1
fi
echo "publish/wait scripts: all cases passed"
