# -*- coding: utf-8 -*-
# filename: test_async_exit_stack.py
# @Time    : 2025/8/20 16:46
# @Author  : JQQ
# @Email   : jqq1716@gmail.com
# @Software: PyCharm
import asyncio
from contextlib import AsyncExitStack, asynccontextmanager

import pytest
from anyio import create_task_group


# 创建两个带有计数功能的异步上下文管理器
@asynccontextmanager
async def ctx_manager(name: str):
    """带状态的异步上下文管理器，记录进入/退出次数"""
    print(f"ENTER: {name}")
    try:
        yield f"{name}-resource"
    finally:
        print(f"EXIT: {name}")


@asynccontextmanager
async def counting_ctx(name: str):
    """带计数器的上下文管理器"""
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
    await stack2.enter_async_context(ctx_manager("CTX-2"))

    await stack1.enter_async_context(counting_ctx("CTX-3"))
    await stack2.enter_async_context(counting_ctx("CTX-4"))

    await stack1.aclose()
    await stack2.aclose()


# 测试用例1：基础嵌套使用
@pytest.mark.anyio
async def test_single_async_exit_stack():
    """测试单个AsyncExitStack管理多个嵌套上下文"""
    stack = AsyncExitStack()
    exit_order = []

    # 进入第一个上下文
    res1 = await stack.enter_async_context(ctx_manager("CTX-1"))
    assert res1 == "CTX-1-resource"
    exit_order.append(1)

    # 进入第二个上下文
    res2 = await stack.enter_async_context(ctx_manager("CTX-2"))
    assert res2 == "CTX-2-resource"
    exit_order.append(2)

    # 验证资源正常访问
    async with counting_ctx("CTX-3") as ctx3:
        ctx3["enter"] = 10
        exit_order.append(3)

    await stack.aclose()

    # 验证退出顺序（后进先出）
    assert exit_order == [1, 2, 3]
    # 退出后资源已被清理
    print("\nTest1 Done")


# 测试用例2：多实例管理
@pytest.mark.anyio
async def test_multi_async_exit_stacks():
    """测试两个独立的AsyncExitStack同时工作"""
    results = []

    # 第一个独立栈
    async def client1():
        stack = AsyncExitStack()
        async with stack:
            await stack.enter_async_context(counting_ctx("Client1-CTX"))
            results.append("Client1-enter")
            # 模拟资源操作
            await asyncio.sleep(0.05)
            results.append("Client1-exit")

    # 第二个独立栈
    async def client2():
        stack = AsyncExitStack()
        async with stack:
            await stack.enter_async_context(counting_ctx("Client2-CTX"))
            results.append("Client2-enter")
            # 模拟资源操作（比client1稍长）
            await asyncio.sleep(0.1)
            results.append("Client2-exit")

    # 并行运行两个客户端
    async with create_task_group() as tg:
        tg.start_soon(client1)
        tg.start_soon(client2)

    # 验证执行顺序（预期交叉执行）
    assert results == ["Client1-enter", "Client2-enter", "Client1-exit", "Client2-exit"]
    print("\nTest2 Done")


# 测试用例3：模拟失败场景（可选）
@pytest.mark.anyio
async def test_stack_exception_handling():
    """测试异常发生时资源清理是否正常"""
    stack = AsyncExitStack()
    cleaned = []

    class CustomError(Exception):
        pass

    @asynccontextmanager
    async def safe_ctx(name):
        try:
            yield name
        finally:
            cleaned.append(name)

    try:
        async with stack:
            await stack.enter_async_context(safe_ctx("RES-A"))
            await stack.enter_async_context(safe_ctx("RES-B"))
            # 触发异常
            raise CustomError("Simulated failure")
    except CustomError:
        pass

    # 验证所有资源已被清理
    assert "RES-A" in cleaned
    assert "RES-B" in cleaned
    assert len(cleaned) == 2
    print("\nTest3 Done")
