# Nitro Enclave 可验证计算 —— 架构设计需求

## 1. 项目概述

在 AWS Nitro Enclave 中运行一个 Rust 程序，对公开的 JSON 输入执行确定性计算，产出 JSON 输出，并生成一份密码学证明包，使任意不信任运行者的第三方都能验证「该输出确实由指定代码、在可信执行环境中、基于该输入计算得出」。

当前阶段使用 mock 程序验证整体流程，后续替换为真实算法。

## 2. 术语定义

| 术语 | 含义 |
|------|------|
| Host | 运行 Enclave 的 EC2 父实例 |
| Enclave | Nitro Enclave 隔离环境，无持久化存储、无网络，仅通过 vsock 与 Host 通信 |
| EIF | Enclave Image File，由 Docker 镜像转换而来的 Enclave 可执行镜像 |
| PCR | Platform Configuration Register，Enclave 启动时度量的哈希值，PCR0 = EIF 镜像哈希，PCR1 = Linux 内核及引导哈希，PCR2 = 应用程序哈希 |
| Attestation Document | Nitro Hypervisor 签发的证明文件，包含 PCR 值、可选 user_data、nonce、public_key，由 AWS Nitro Attestation PKI 签名 |
| vsock | Virtio socket，Host 与 Enclave 之间唯一的通信通道 |
| Proof Package | 交付给验证者的完整证明材料集合 |

## 3. 威胁模型与信任假设

**防范目标**：向不信任运行者的第三方证明计算结果的真实性。

**信任假设**：

- 信任 AWS Nitro Hypervisor 的完整性（Attestation Document 的签名根）
- 信任 Rust 程序源码是公开可审计的
- 不要求信任运行者（运行者无法在 Enclave 内篡改计算过程）

**不在范围内**：

- 不需要保护输入的机密性（输入是公开的）
- 不需要防范 AWS 自身的恶意行为（超出 Nitro Enclave 设计边界）

## 4. 系统架构

```
┌─────────────────────────────────────────────┐
│                 EC2 Host                     │
│                                              │
│  ┌──────────┐     vsock      ┌────────────┐ │
│  │ Host App │◄══════════════►│  Enclave   │ │
│  │          │   (CID:X,      │            │ │
│  │ 发送input│    Port:5000)  │ Rust 程序  │ │
│  │ 接收output│               │ (mock)     │ │
│  │ 接收证明  │               │            │ │
│  └──────────┘                └────────────┘ │
└─────────────────────────────────────────────┘
```

**组件职责**：

| 组件 | 职责 |
|------|------|
| Host App | 启动/终止 Enclave；通过 vsock 发送 JSON 输入；接收 JSON 输出和 Attestation Document；组装 Proof Package |
| Enclave 内进程 | 监听 vsock；接收输入；调用 Rust 计算逻辑；将 `SHA-256(input \|\| output)` 写入 Attestation 请求的 user_data 字段；请求 Nitro Hypervisor 签发 Attestation Document；将输出和 Attestation Document 通过 vsock 返回给 Host |
| Rust 计算程序 | 纯函数：接收 JSON 输入，返回 JSON 输出。当前为 mock 实现，后续替换 |

## 5. 执行流程（One-Shot 模式）

```
Host                              Enclave
 │                                   │
 │  1. nitro-cli run-enclave         │
 │──────────────────────────────────►│
 │                                   │  2. 启动，监听 vsock
 │                                   │
 │  3. 通过 vsock 发送 JSON input    │
 │──────────────────────────────────►│
 │                                   │  4. 调用 Rust 程序计算 output
 │                                   │  5. 计算 SHA-256(input || output)
 │                                   │  6. 以该 hash 为 user_data，
 │                                   │     请求 Attestation Document
 │                                   │
 │  7. 通过 vsock 返回:              │
 │     - JSON output                 │
 │     - Attestation Document (CBOR) │
 │◄──────────────────────────────────│
 │                                   │
 │  8. nitro-cli terminate-enclave   │
 │──────────────────────────────────►│
 │                                   │
 │  9. 组装 Proof Package            │
 │                                   │
```

## 6. Attestation 设计

**user_data 绑定方案**：

Enclave 内进程在请求 Attestation Document 时，将以下值写入 `user_data` 字段：

```
user_data = SHA-256(input_bytes || output_bytes)
```

其中 `input_bytes` 和 `output_bytes` 是原始 JSON 的 UTF-8 字节序列。

这实现了三方绑定：
- **代码身份**：PCR0/PCR1/PCR2 锁定了 EIF 镜像（即 Rust 程序和运行环境）
- **输入输出**：user_data 中的 hash 锁定了具体的输入和输出
- **可信来源**：Attestation Document 由 Nitro Hypervisor 签名，证明以上数据确实来自 Enclave 内部

**PCR 值与镜像的关系**：

PCR0 在 `nitro-cli build-enclave` 时确定性地由 Docker 镜像内容计算得出。相同的 Docker 镜像必然产生相同的 PCR0。验证者可通过重建镜像来独立计算预期的 PCR0 值。

## 7. 证明包（Proof Package）定义

交付给验证者的证明包包含以下内容：

| 字段 | 格式 | 说明 |
|------|------|------|
| `attestation_document` | CBOR (Base64 编码) | Nitro Hypervisor 签发的原始 Attestation Document |
| `output` | JSON | Rust 程序的计算输出 |
| `docker_image_info` | 对象 | 包含：Dockerfile 内容或引用、源码仓库地址及 commit hash、构建指令、预期的 PCR0 值 |

验证者同时持有公开的 input，因此 input 不包含在证明包中，但需要在证明包之外单独提供或约定获取方式。

## 8. 验证流程

### 路径 A：Attestation 验证（轻量级，无需自建 Enclave）

1. 解析 Attestation Document（COSE Sign1 结构）
2. 验证签名链：叶证书 → 中间 CA → AWS Nitro Attestation Root CA
3. 检查 Attestation Document 中的 PCR0 是否与预期值一致（预期值来自 `docker_image_info` 或验证者自行构建镜像后用 `nitro-cli build-enclave --output-file /dev/null` 获取）
4. 用持有的 input 和证明包中的 output 重新计算 `SHA-256(input || output)`，验证是否与 Attestation Document 中的 user_data 一致
5. 若以上全部通过，则证明：该输出由指定镜像在 Enclave 中基于该输入计算得出

### 路径 B：重新执行验证（完整验证，需要自建 Enclave）

1. 从 `docker_image_info` 中的源码仓库和 commit hash 重建 Docker 镜像
2. 使用 `nitro-cli build-enclave` 生成 EIF，确认 PCR0 与证明包中声明的一致
3. 在自己的 Nitro Enclave 实例中启动该 EIF
4. 传入相同的 input
5. 对比输出结果是否与证明包中的 output 完全一致
6. （可选）对比自己获得的 Attestation Document 中的 user_data 是否一致

## 9. 可复现构建要求

为确保验证者能从源码重建出相同的 Docker 镜像（进而得到相同的 PCR0），必须满足：

- **固定基础镜像**：Dockerfile 中使用带摘要（digest）的基础镜像引用，如 `FROM rust:1.XX@sha256:abc...`
- **锁定依赖版本**：使用 `Cargo.lock` 并将其纳入版本控制
- **确定性构建顺序**：避免 Dockerfile 中引入时间戳、随机数等不确定因素
- **记录完整构建命令**：在 `docker_image_info` 中提供精确的 `docker build` 命令和参数
- **验证流程文档化**：提供从 `git clone` 到获得 PCR0 值的完整步骤

## 10. 约束与假设

- **最简原则**：整个方案只服务于「可验证计算」这一目标，不引入不必要的中间件、数据库或额外服务
- **单次执行**：Enclave 启动后处理一次请求即终止，不维护长连接或状态
- **Mock 程序**：当前阶段 Rust 程序为 mock 实现（如简单的 JSON 字段变换），目的是验证 Enclave 通信、Attestation 签发和验证的完整链路。架构设计须保证 mock 可被无缝替换为真实算法
- **输入公开**：输入数据不需要保密，验证者可获取完整输入
- **确定性算法**：Rust 程序必须是确定性的 —— 相同输入始终产生相同输出（不依赖时间、随机数或外部状态）
