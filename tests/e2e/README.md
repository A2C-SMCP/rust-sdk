# E2E Tests for SMCP Protocol

This directory contains end-to-end tests for the SMCP (Server-Computer-Agent) protocol implementation.

## Structure

```
tests/e2e/
├── mod.rs           # Module exports
├── helpers.rs       # Helper functions and utilities
├── mock_agent.rs    # Mock Agent event handler for testing
├── test_server.rs   # Test server wrapper
└── README.md        # This file

tests/
├── e2e_minimal_test.rs    # Minimal working E2E tests
├── e2e_basic_test.rs      # Basic integration tests
├── e2e_integration_test.rs # Full integration tests
├── e2e_tool_call_test.rs  # Tool call specific tests
├── e2e_desktop_test.rs    # Desktop sync tests
└── e2e_multi_computer_test.rs # Multi-computer scenarios
```

## Running the Tests

### Run all E2E tests:
```bash
cargo test --features full --test e2e_minimal_test
cargo test --features full --test e2e_basic_test
cargo test --features full --test e2e_integration_test
```

### Run specific test:
```bash
cargo test --features full test_minimal_server_startup -- --nocapture
```

### Run with output:
```bash
cargo test --features full --test e2e_minimal_test -- --nocapture
```

## Test Categories

### 1. Minimal Tests (`e2e_minimal_test.rs`)
- Basic server startup
- Computer connection to server
- Agent connection to server

These tests verify the most basic functionality and are good for quick smoke testing.

### 2. Basic Tests (`e2e_basic_test.rs`)
- Server startup
- Computer lifecycle (boot, connect, join)
- Agent lifecycle (connect, join)
- Basic three-component integration

### 3. Integration Tests (`e2e_integration_test.rs`)
- Full three-component flow
- Tool discovery and retrieval
- Notification system
- Proper cleanup

### 4. Tool Call Tests (`e2e_tool_call_test.rs`)
- Tool invocation
- Concurrent tool calls
- Tool call timeout handling
- Error scenarios

### 5. Desktop Tests (`e2e_desktop_test.rs`)
- Desktop information retrieval
- Desktop update notifications
- Resource subscription

### 6. Multi-Computer Tests (`e2e_multi_computer_test.rs`)
- Agent with multiple computers
- Computer join/leave notifications
- Room/session listing

## Key Components

### TestServer
Wrapper around the SMCP server that handles:
- Automatic port allocation
- Background task spawning
- Lifecycle management

### MockEventHandler
Event handler implementation that:
- Captures all events for verification
- Provides waiting utilities
- Enables assertion-based testing

### Helper Functions
- `generate_office_id()` - Create unique office IDs
- `generate_agent_name()` - Create unique agent names
- `generate_computer_name()` - Create unique computer names
- `wait_for_condition()` - Async condition waiting with timeout

## Design Principles

1. **Isolation**: Each test is self-contained and cleans up after itself
2. **Parallel-safe**: Tests use random IDs to enable parallel execution
3. **Real components**: Tests use actual Server/Agent/Computer implementations, not mocks
4. **Timeout-aware**: All async operations have reasonable timeouts
5. **Clear errors**: Failures include descriptive error messages

## Future Enhancements

- [ ] Add performance benchmarking tests
- [ ] Add stress tests with many concurrent agents/computers
- [ ] Add network failure simulation tests
- [ ] Add security/auth tests
- [ ] Add MCP server integration tests with real MCP servers

## Troubleshooting

### Tests fail with "Address already in use"
This shouldn't happen as tests use random ports, but if it does:
```bash
lsof -i :3000  # Check what's using the port
```

### Tests timeout
Increase timeout in the test or check for:
- Server not starting properly
- Network connectivity issues
- Resource exhaustion

### Agent doesn't detect Computer
Check:
- Both are using the same `office_id`
- Server is running and accessible
- Firewall isn't blocking connections
