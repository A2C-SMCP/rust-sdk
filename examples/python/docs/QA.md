# 在使用A2C python-sdk 过程中常见问题及解法

## 1. “当前工具需要调用前进行二次确认，但客户端目前没有实现二次确认回调方法。请联系用户反馈此问题”

一般而言如果遇到：

{
  "meta": null,
  "content": [
    {
      "type": "text",
      "text": "当前工具需要调用前进行二次确认，但客户端目前没有实现二次确认回调方法。请联系用户反馈此问题",
      "annotations": null,
      "meta": null
    }
  ],
  "structuredContent": null,
  "isError": true
}

说明当前被调用的工具在添加服务时启用了二次确认能力，如果在cli测试过程中遇到此问题，想要关闭，有两个方法：

a. 可以在配置文件中 tool_meta.{工具名} 设置 auto_apply: true
b. 可以在配置文件中 default_tool_meta.auto_apply: true，关闭当前服务所有未显式声明打开的工具的二次确认

