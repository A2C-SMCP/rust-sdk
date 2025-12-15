# -*- coding: utf-8 -*-
# filename: test_async_exit_stack_anyio.py
# @Time    : 2025/8/20 16:46
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
"""
这一组用例相较于 @test_async_exit_stack.py 主要是引入了anyio的task_group

本文件与 test_async_exit_stack.py 的主要区别：
1. 这里所有异步上下文管理器都在 anyio 的 TaskGroup 中运行。
2. 使用 TaskGroup 后，任务的调度顺序由 anyio 控制，和普通的 asyncio 行为有细微差异。
3. 由于调度顺序的不确定性，某些资源的 enter/exit 顺序与传统 AsyncExitStack 有不同，测试结果可能不同。

【调用顺序说明】
- 在 test_async_exit_stack.py 中，多个上下文的 enter/exit 顺序严格受控于 AsyncExitStack 的入栈/出栈顺序。
- 在本文件中，TaskGroup 可能并发启动上下文，导致 enter/exit 顺序不再严格一致。
- 这种差异在资源释放（exit）阶段尤为明显，尤其是当有异常或并发任务时。

【建议】
- 如果你需要严格的上下文 enter/exit 顺序，建议不要混用 TaskGroup 和 AsyncExitStack。
- 如果你需要高并发，且资源释放顺序无关紧要，可以采用本文件的写法。

This test suite differs from @test_async_exit_stack.py mainly by introducing anyio's task_group.

Key differences:
1. All async context managers run inside anyio's TaskGroup.
2. TaskGroup schedules tasks concurrently, so the order of enter/exit may differ from pure asyncio+AsyncExitStack.
3. Because of this, test results may vary—especially for enter/exit order—compared to traditional AsyncExitStack usage.

[Order Explanation]
- In test_async_exit_stack.py, enter/exit order is strictly controlled by AsyncExitStack push/pop order.
- Here, TaskGroup may start contexts concurrently, so order is not guaranteed.
- This is most obvious during resource exit, especially with exceptions or concurrency.

[Recommendation]
- If you need strict enter/exit order, avoid mixing TaskGroup and AsyncExitStack.
- If you want high concurrency and don't care about resource exit order, this pattern is fine.
"""

from contextlib import AsyncExitStack, asynccontextmanager

import anyio
import pytest


# 创建两个带有计数功能的异步上下文管理器
@asynccontextmanager
async def ctx_manager(name: str):
    """带状态的异步上下文管理器，记录进入/退出次数"""
    async with anyio.create_task_group() as _task_group:
        print(f"ENTER: {name}")
        try:
            yield f"{name}-resource"
        finally:
            print(f"EXIT: {name}")


@asynccontextmanager
async def counting_ctx(name: str):
    """带计数器的上下文管理器"""
    async with anyio.create_task_group() as _task_group:
        state = {"enter": 0, "exit": 0}
        print(f"CREATE: {name}")

        try:
            state["enter"] += 1
            print(f"ENTER: {name} (count={state['enter']})")
            yield state
        finally:
            state["exit"] += 1
            print(f"EXIT: {name} (count={state['exit']})")


@pytest.mark.anyio
async def test_async_exit_stack_zipper_merge():
    stack1 = AsyncExitStack()
    stack2 = AsyncExitStack()

    await stack1.enter_async_context(ctx_manager("CTX-1"))

    await stack1.enter_async_context(counting_ctx("CTX-3"))
    await stack2.enter_async_context(ctx_manager("CTX-2"))
    await stack2.enter_async_context(counting_ctx("CTX-4"))

    await stack2.aclose()
    await stack1.aclose()


@pytest.mark.xfail
@pytest.mark.anyio
async def test_async_exit_stacks_zipper_merge_failed0():
    """测试两个独立的AsyncExitStack同时工作"""
    stack1 = AsyncExitStack()
    stack2 = AsyncExitStack()

    await stack1.enter_async_context(ctx_manager("CTX-1"))
    await stack2.enter_async_context(ctx_manager("CTX-2"))

    await stack1.enter_async_context(counting_ctx("CTX-3"))
    await stack2.enter_async_context(counting_ctx("CTX-4"))

    await stack1.aclose()
    await stack2.aclose()


@pytest.mark.xfail
@pytest.mark.anyio
async def test_async_exit_stacks_zipper_merge_failed1():
    """测试两个独立的AsyncExitStack同时工作"""
    stack1 = AsyncExitStack()
    stack2 = AsyncExitStack()

    await stack1.enter_async_context(ctx_manager("CTX-1"))
    await stack2.enter_async_context(ctx_manager("CTX-2"))

    await stack1.enter_async_context(counting_ctx("CTX-3"))
    await stack2.enter_async_context(counting_ctx("CTX-4"))

    await stack1.aclose()
    await stack2.aclose()
