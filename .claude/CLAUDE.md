# 项目环境常用命令

## 测试相关别名 / Test aliases

test-ws = "test --workspace"
test-all = "test --workspace --all-features"
test-e2e = "test --workspace --features e2e -- --ignored"
test-computer = "test -p smcp-computer"
test-agent = "test -p smcp-agent"
test-server = "test -p smcp-server-core"

# 代码质量别名 / Code quality aliases

fmt-all = "fmt --all"
clippy-workspace = "clippy --workspace --all-targets --all-features -- -D warnings"
clippy-loose = "clippy --workspace --all-targets --all-features"
