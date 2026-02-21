# Nitro Enclave Verifiable Computation

在 AWS Nitro Enclave 中运行确定性计算，生成密码学证明，使任意第三方无需信任运行者即可验证结果。

## 目录结构

```
.
├── enclave-app/        # Enclave 内运行的 Rust 程序
├── host-app/           # EC2 Host 端程序，负责启动 Enclave 并收集证明
├── verifier/           # 离线验证脚本（Python），任何人可在本地运行
├── build.sh            # 构建 Docker 镜像 + EIF + 记录 PCR 值
├── run_demo.sh         # 一键运行完整流程
├── test_input.json     # 示例输入
├── output_aws/         # 真实 Nitro Enclave 产出的示例 proof
├── requirements.md     # 架构设计需求文档
└── plan.md             # 实施计划
```

### enclave-app/

在 Enclave 隔离环境中运行的 Rust 程序。通过 vsock 接收 JSON 输入，执行计算（当前为 mock 求和），将 `SHA-256(input || output)` 写入 Attestation 的 `user_data` 字段，请求 Nitro Security Module 签发 Attestation Document，通过 vsock 返回结果。

### host-app/

在 EC2 父实例上运行的 Rust 程序。负责启动 Enclave、通过 vsock 发送输入、接收输出和 Attestation Document、终止 Enclave，最终组装 Proof Package。

### verifier/

Python 验证脚本，执行三项检查：
1. **签名验证** — COSE Sign1 签名链 → AWS Nitro Root CA
2. **代码身份** — PCR0 与预期值比对（确认运行的是指定代码）
3. **数据完整性** — 重算 `SHA-256(input || output)` 与 `user_data` 比对

---

## 快速开始

### 前提条件

- AWS 账号，能创建支持 Nitro Enclave 的 EC2 实例（如 `m5.xlarge`）
- 本地安装 AWS CLI、SSH

### 1. 启动 EC2 实例

```bash
aws ec2 run-instances \
  --image-id ami-0ac0e4288aa341886 \
  --instance-type m5.xlarge \
  --key-name your-key \
  --enclave-options 'Enabled=true' \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=nitro-demo}]'
```

### 2. 配置实例

SSH 进入后：

```bash
# 安装工具
sudo dnf install -y aws-nitro-enclaves-cli aws-nitro-enclaves-cli-devel docker gcc
sudo usermod -aG ne,docker $USER
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# 配置 Enclave 资源
sudo tee /etc/nitro_enclaves/allocator.yaml <<EOF
---
memory_mib: 512
cpu_count: 2
EOF

sudo systemctl enable --now nitro-enclaves-allocator docker

# 重新登录使 group 生效
exit
```

### 3. 构建

```bash
git clone <this-repo>
cd <this-repo>
chmod +x build.sh run_demo.sh

# 构建 Docker 镜像 → EIF → 记录 PCR 值
sudo ./build.sh
```

输出示例：

```
PCR0: c37a57f399e452eeb7302f6ffc192e1e6bad86d93aef36e6378ce05165c0d0d76ce6a463d6ebd02e06b3a9e21f0e84ec
PCR1: 4b4d5b3661b3efc12920900c80e126e4ce783c522de6c02a2a5bf7af3a2b9327b86776f188e4be1c1c404a129dbda493
PCR2: b2d9adaa59be259bf6e3f36f43eb6b3a5428ba30a15ba0a1201ed58233609e3fc4a91cca99f6cde67559c5e717845e08
```

记下 **PCR0**，这是 Enclave 镜像的唯一身份标识。

### 4. 运行计算

```bash
# 构建 Host 程序
cd host-app && cargo build --release && cd ..

# 运行 Enclave 计算
sudo ./host-app/target/release/host-app enclave.eif test_input.json ./output
```

输入（`test_input.json`）：

```json
{
  "values": [1, 2, 3],
  "operation": "sum"
}
```

输出目录 `./output/` 包含：
- `proof_package.json` — 完整证明包
- `input.json` — 输入副本

### 5. 验证（任何机器）

将 `proof_package.json` 和 `input.json` 拷贝到任何机器，无需 Enclave：

```bash
cd verifier
pip install -r requirements.txt

# 验证
python verify.py ../output/proof_package.json ../output/input.json \
  --expected-pcr0 c37a57f399e452eeb7302f6ffc192e1e6bad86d93aef36e6378ce05165c0d0d76ce6a463d6ebd02e06b3a9e21f0e84ec
```

输出：

```
[+] Attestation document parsed successfully
[+] Certificate chain verified
[+] Signature verified
[+] PCR0 matches expected value
[+] User data matches - input/output integrity verified

RESULT: PASS
```

### 6. 篡改检测示例

```bash
# 修改 proof_package.json 中的 output.result 为 999
python verify.py tampered_proof.json input.json --expected-pcr0 <pcr0>

# 输出：
# [!] USER DATA MISMATCH - output may have been tampered with!
# RESULT: FAIL
```

---

## 验证 Proof Package 的含义

当 `verify.py` 输出 **PASS**，意味着：

1. 该 Attestation Document 确实由 AWS Nitro Hypervisor 签发（不可伪造）
2. 签发时运行的代码镜像 hash 与 PCR0 一致（代码身份确认）
3. `SHA-256(input || output)` 与 Attestation 中的 `user_data` 一致（输入输出未被篡改）

因此：**该输出确实由指定代码在 Nitro Enclave 中基于该输入计算得出**。

---

## 可复现构建

第三方可从源码重建相同镜像来验证 PCR0：

```bash
git clone <this-repo>
cd <this-repo>

# Dockerfile 使用固定 digest 的基础镜像，Cargo.lock 锁定依赖
docker build --platform linux/amd64 -t enclave-app:v1 ./enclave-app/
nitro-cli build-enclave --docker-uri enclave-app:v1 --output-file /dev/null

# 比对输出的 PCR0 是否与声明值一致
```

---

## 通信协议

Host ↔ Enclave 通过 vsock（端口 5000）通信，帧格式：

```
┌──────────────┬──────────────┐
│ 4 bytes BE   │ payload      │
│ (length)     │ (JSON/CBOR)  │
└──────────────┴──────────────┘
```

请求：1 帧（JSON input）
响应：2 帧（JSON output + Attestation Document CBOR）

---

## 替换为真实算法

当前 mock 计算为数组求和。替换步骤：

1. 修改 `enclave-app/src/main.rs` 中的 `Input`/`Output` 结构体和计算逻辑
2. 保持确定性（相同输入 → 相同输出，不依赖时间/随机数）
3. 重新 `build.sh`，记录新的 PCR0
4. 其余流程不变

## License

MIT
