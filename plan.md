# 实施计划

基于 `requirements.md` 编写，目标是在 AWS Nitro Enclave 中跑通完整的可验证计算链路（mock 版本）。

---

## 贯穿约束：可复现构建

以下要求贯穿所有阶段，不是最后才处理的事项：

1. **Dockerfile 基础镜像必须用 digest 固定**：如 `FROM rust:1.82.0-slim@sha256:abcdef...`，禁止裸 tag
2. **Cargo.lock 必须提交到版本控制**，构建时使用 `--locked` 标志
3. **Dockerfile 中消除不确定性**：
   - 设置 `ENV SOURCE_DATE_EPOCH=0` 消除时间戳差异
   - 使用 `COPY` 而非 `ADD`（避免 URL 拉取的不确定性）
   - `RUN` 指令中不使用 `apt-get upgrade` 等滚动更新命令
   - 指定 `--platform linux/amd64` 固定构建架构
4. **提供 `build.sh` 脚本**：封装从源码到 PCR0 的完整路径
   ```bash
   #!/bin/bash
   set -euo pipefail
   docker build --platform linux/amd64 -t enclave-app:v1 ./enclave-app/
   nitro-cli build-enclave --docker-uri enclave-app:v1 --output-file enclave.eif
   # 输出 PCR0/1/2
   ```
5. **`docker_image_info` 在阶段 2 就生成**（不留到阶段 5）：Host App 构建 Proof Package 时自动填入 Dockerfile 内容、git commit hash、构建命令、PCR0
6. **验证者复现文档**：`README.md` 中包含从 `git clone` 到比对 PCR0 的逐步指南

---

## 阶段 0：EC2 环境准备

**目标**：启动一台支持 Nitro Enclave 的 EC2 实例，安装所有必要工具。

**步骤**：

1. 使用 `aws ec2 run-instances` 启动实例，要求：
   - 实例类型：`m5.xlarge`（或其他支持 Enclave 的类型，至少 4 vCPU 以便分配 2 个给 Enclave）
   - AMI：Amazon Linux 2023（x86_64）
   - `--enclave-options Enabled=true`
   - 安全组开放 SSH（22 端口）
2. SSH 连接到实例
3. 安装 Nitro Enclaves CLI：
   - `sudo amazon-linux-extras install aws-nitro-enclaves-cli -y`（AL2）或 `sudo dnf install aws-nitro-enclaves-cli aws-nitro-enclaves-cli-devel -y`（AL2023）
   - `sudo usermod -aG ne ec2-user`
   - `sudo usermod -aG docker ec2-user`
4. 配置 Enclave 资源分配（`/etc/nitro_enclaves/allocator.yaml`）：
   - `memory_mib: 512`
   - `cpu_count: 2`
5. 启动服务：
   - `sudo systemctl enable --now nitro-enclaves-allocator.service`
   - `sudo systemctl enable --now docker`
6. 重新登录（使 group 生效），验证 `nitro-cli --version` 和 `docker info` 正常

**交付物**：一台可用的 EC2 实例，`nitro-cli` 和 `docker` 就绪

**完成标准**：`nitro-cli describe-enclaves` 返回空列表（无报错）

**预计耗时**：15 分钟

---

## 阶段 1：Enclave 内 Rust 程序（Mock）

**目标**：编写 Enclave 内运行的 Rust 程序，实现 vsock 监听、mock 计算、Attestation 请求。

**步骤**：

1. 在 `rust-tee-demo/enclave-app/` 下创建 Rust 项目（`cargo init`）
2. 依赖：
   - `serde` + `serde_json`：JSON 序列化
   - `aws-nitro-enclaves-nsm-api`：与 Nitro Security Module 交互，请求 Attestation Document
   - `nix` 或手动 `libc` 绑定：vsock 支持（`AF_VSOCK`）
   - `sha2`：计算 SHA-256
3. 实现逻辑：
   - 启动后监听 vsock（端口 5000，CID = `VMADDR_CID_ANY`）
   - 接收 JSON input（带长度前缀的帧协议，如 4 字节大端长度 + payload）
   - Mock 计算：对输入 JSON 做简单变换（如添加 `"processed": true` 字段、计算字段求和等）
   - 计算 `user_data = SHA-256(input_bytes || output_bytes)`
   - 通过 NSM API 请求 Attestation Document，将 `user_data` 传入
   - 将 output JSON 和 Attestation Document（CBOR 原始字节）通过 vsock 返回给 Host
4. 编写 `Dockerfile`（多阶段构建）：
   - 构建阶段：`rust:1.XX@sha256:...`（固定 digest），编译 Rust 程序为静态二进制
   - 运行阶段：最小基础镜像（如 `scratch` 或 `alpine`），仅包含编译后的二进制
   - 入口点为该 Rust 二进制
5. 将 `Cargo.lock` 纳入版本控制

**交付物**：
- `enclave-app/src/main.rs`
- `enclave-app/Cargo.toml` + `Cargo.lock`
- `enclave-app/Dockerfile`

**完成标准**：`docker build` 成功；`nitro-cli build-enclave --docker-uri <image> --output-file enclave.eif` 成功并输出 PCR0/1/2

**预计耗时**：45 分钟

---

## 阶段 2：Host App

**目标**：编写 Host 端程序，负责与 Enclave 通信并组装 Proof Package。

**步骤**：

1. 在 `rust-tee-demo/host-app/` 下创建 Rust 项目（或使用 Python/Bash 脚本——选择 Rust 以保持一致性）
2. 依赖：
   - `serde` + `serde_json`
   - `nix` 或 `libc`：vsock 客户端连接
   - `base64`：编码 Attestation Document
3. 实现逻辑：
   - 接收命令行参数：EIF 路径、JSON input 文件路径、输出目录
   - 调用 `nitro-cli run-enclave` 启动 Enclave（通过 `std::process::Command`），记录 EnclaveCID
   - 通过 vsock 连接到 Enclave（CID 从 run-enclave 输出中解析，端口 5000）
   - 发送 JSON input（帧协议与 Enclave 端一致）
   - 接收 output JSON 和 Attestation Document
   - 调用 `nitro-cli terminate-enclave` 终止 Enclave
   - 组装 Proof Package（JSON 文件），包含：
     - `attestation_document`（Base64 编码的 CBOR）
     - `output`（JSON 对象）
     - `docker_image_info`（Dockerfile 内容、仓库地址、commit hash、构建命令、PCR0）
   - 将 Proof Package 写入输出目录

**交付物**：
- `host-app/src/main.rs`
- `host-app/Cargo.toml` + `Cargo.lock`

**完成标准**：在 EC2 上运行 Host App，传入测试 input，成功获得 Proof Package JSON 文件

**预计耗时**：40 分钟

---

## 阶段 3：验证脚本（路径 A）

**目标**：实现轻量级验证——不需要 Enclave，任何人可在本地运行。

**步骤**：

1. 在 `rust-tee-demo/verifier/` 下创建项目（推荐 Python，因为 CBOR/COSE 库成熟）
2. 依赖：
   - `cbor2`：解析 CBOR
   - `pycose`：验证 COSE Sign1 签名
   - `cryptography`：证书链验证
   - `hashlib`：SHA-256
3. 实现逻辑：
   - 输入：Proof Package JSON + input JSON
   - 解析 Attestation Document（COSE Sign1 → CBOR payload）
   - 从 payload 中提取证书链、PCR 值、user_data
   - 验证签名链：叶证书 → 中间 CA → AWS Nitro Attestation Root CA
     - Root CA 证书从 AWS 官方下载：`https://aws-nitro-enclaves.amazonaws.com/AWS_NitroEnclaves_Root-G1.zip`
   - 比对 PCR0 与 `docker_image_info` 中声明的预期值
   - 重新计算 `SHA-256(input_bytes || output_bytes)`，与 user_data 比对
   - 输出验证结果（通过/失败 + 详细原因）

**交付物**：
- `verifier/verify.py`
- `verifier/requirements.txt`
- `verifier/AWS_NitroEnclaves_Root-G1.pem`（Root CA）

**完成标准**：对阶段 2 产出的 Proof Package 运行验证脚本，输出 PASS；篡改 output 后重新运行，输出 FAIL

**预计耗时**：30 分钟

---

## 阶段 4：端到端集成测试

**目标**：在 EC2 上跑通完整流程，验证所有环节。

**步骤**：

1. 准备测试 input（`test_input.json`），内容如 `{"values": [1, 2, 3], "operation": "sum"}`
2. 在 EC2 上执行完整流程：
   - `docker build` → `nitro-cli build-enclave` → 记录 PCR0
   - 运行 Host App → 获得 Proof Package
3. 将 Proof Package 和 input 拷贝到本地（或另一台机器）
4. 运行验证脚本（路径 A），确认 PASS
5. 篡改测试：
   - 修改 output 中的值 → 验证应 FAIL（user_data 不匹配）
   - 修改 PCR0 → 验证应 FAIL（PCR 不匹配）
6. （可选）路径 B 验证：在另一台 Enclave 实例上从源码重建镜像，确认 PCR0 一致，重跑获得相同 output

**交付物**：
- `test_input.json`
- `run_demo.sh`：一键执行脚本（build → run → verify）
- `README.md`：完整的使用说明

**完成标准**：`run_demo.sh` 一键跑通，验证脚本对正确数据输出 PASS，对篡改数据输出 FAIL

**预计耗时**：30 分钟

---

## 阶段 5：文档与清理

**目标**：补充文档，确保项目可交付给第三方复现。

**步骤**：

1. 完善 `README.md`：项目介绍、前置条件、快速开始、架构图、验证步骤
2. 在 `docker_image_info` 中记录：
   - Dockerfile 内容
   - Git 仓库地址 + commit hash
   - 构建命令（`docker build -t enclave-app:v1 .`）
   - PCR0 值
3. 确认所有依赖版本已锁定（Cargo.lock、requirements.txt、Dockerfile 中的 digest）
4. 代码整理，删除调试用的临时文件

**交付物**：完整的 `rust-tee-demo/` 目录，可独立 clone 和运行

**完成标准**：按 README 从零开始可复现整个流程

**预计耗时**：20 分钟

---

## 项目目录结构（预期）

```
rust-tee-demo/
├── README.md
├── plan.md
├── test_input.json
├── run_demo.sh
├── enclave-app/
│   ├── Cargo.toml
│   ├── Cargo.lock
│   ├── Dockerfile
│   └── src/
│       └── main.rs
├── host-app/
│   ├── Cargo.toml
│   ├── Cargo.lock
│   └── src/
│       └── main.rs
└── verifier/
    ├── verify.py
    ├── requirements.txt
    └── AWS_NitroEnclaves_Root-G1.pem
```

## 总预计耗时

| 阶段 | 耗时 |
|------|------|
| 阶段 0：EC2 环境准备 | 15 min |
| 阶段 1：Enclave Rust 程序 | 45 min |
| 阶段 2：Host App | 40 min |
| 阶段 3：验证脚本 | 30 min |
| 阶段 4：集成测试 | 30 min |
| 阶段 5：文档清理 | 20 min |
| **合计** | **~3 小时** |
