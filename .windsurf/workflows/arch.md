---
description: 项目架构
---

当前项目提供 A2C-SMCP 协议的rust版本实现。A2C-SMCP是当前团队开源的一个服务协议，但目前协议仍在开发中，因此其具体实现细节需要在：

@examples/python

这个目录下了解（这是PythonSDK，已经基本完成初始版本的开发，为方便阅读我直接复制相关代码到当前项目内参考）

Rust目前的使命就是复制当前Python的能力

1. A2C-SMCP有三个大角色：Server/Agent/Computer。三者使用Socket.IO通信
2. 项目需要提供完整的单元测试/集成测试/e2e测试

目前的项目依赖与规划见项目根目录的 @README.md + @DEV.md

1. A2C-SMCP是跨语言的协议，未来很有可能Python的Computer实现。连接到Rust的Server上，再与TypeScript的Agent通信。
2. 当前版本以Python的实现为准，不要随意修改，因为Python已经通过了业务验证。如果有质疑，可以提出，但不要随意臆造。

我们要求三者均是消息兼容的。因此在接口返回和入参的数据结构上，要尽量避开语言自己的风格与特色，实现标准的协议。

另外需要关注，目前版本的 rust-socketio 开源版本尚不支持 Server->Client ACK消息，而我们使用vendor在当前项目内实现的版本已经实现了此能力。但注意这个能力虽然已经有用例覆盖，但尚未经过生产验证，因此你可以使用，但如果遇到可疑问题，需要反馈给开发者，联合检查是否是当前的ACK机制实现有问题。另外rust-socketio的Payload也发生了变化（因为要响应ACK），具体使用方法可以在项目内参考 smcp-server-xx 部分的相关实现。