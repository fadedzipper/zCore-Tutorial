# 内核对象

Zircon 是一个基于内核对象的系统。系统的功能被划分到若干组内核对象中。

作为一切的开始，本章首先构造了一个内核对象框架，作为后面实现的基础。
然后我们实现第一个内核对象 —— `Process`，它是所有对象的容器，也是将来我们操作对象的入口点。
最后会实现一个稍微复杂但是极其重要的对象 `Channel`，它是进程间通信（IPC）的基础设施，也是传送对象的唯一管道。