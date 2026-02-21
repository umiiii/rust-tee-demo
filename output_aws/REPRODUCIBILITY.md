# 可复现性验证报告

## 测试日期
2026-02-21

## 方法
在全新 EC2 实例上，从 GitHub clone 源码，从零构建并运行，比对结果。

## 环境

| | 第 1 次构建 | 第 2 次构建（复现） |
|---|---|---|
| 实例 | i-0e68559eea0a0c309 | i-055eab0e3c053aeb8 |
| 实例类型 | m5.xlarge | m5.xlarge |
| Region | ap-southeast-1 | ap-southeast-1 |
| AMI | ami-0ac0e4288aa341886 (AL2023) | ami-0ac0e4288aa341886 (AL2023) |
| 源码 commit | 114bcd5 | 114bcd5 |

## 结果

### ✅ 计算输出：完全一致

```json
// 第 1 次
{"processed": true, "result": 6}

// 第 2 次（复现）
{"processed": true, "result": 6}
```

### ✅ user_data hash：完全一致

```
第 1 次: 176646ebb4e3a465f59b93bd455f4c85111c7ede076ea3b4c0aba6670824376a
第 2 次: 176646ebb4e3a465f59b93bd455f4c85111c7ede076ea3b4c0aba6670824376a
```

### ✅ 签名验证：两次均 PASS

### ⚠️ PCR0：不一致

```
第 1 次: c37a57f399e452eeb7302f6ffc192e1e6bad86d93aef36e6378ce05165c0d0d76ce6a463d6ebd02e06b3a9e21f0e84ec
第 2 次: 717d84110f0b3dad533b2a842f133db3d10f02be4352d821353c45665817b717ac14ba13ed95edc0665542c3863191a5
```

### ⚠️ PCR2：不一致

```
第 1 次: b2d9adaa59be259bf6e3f36f43eb6b3a5428ba30a15ba0a1201ed58233609e3fc4a91cca99f6cde67559c5e717845e08
第 2 次: 7f639f8b521f9e0a204911a64a8c603b9486d420446e0af73b1d3fe067079a488955d19d28efd2939b5aae24be66b24c
```

### ✅ PCR1：一致

```
两次均为: 4b4d5b3661b3efc12920900c80e126e4ce783c522de6c02a2a5bf7af3a2b9327b86776f188e4be1c1c404a129dbda493
```

## PCR 不一致分析

- **PCR1**（Linux 内核 + 引导）一致 → 相同 AMI，内核相同
- **PCR0**（EIF 镜像 hash）不一致 → Docker 构建产出了不同的镜像
- **PCR2**（应用程序 hash）不一致 → 与 PCR0 同源

**根因**：Dockerfile 虽然 pin 了基础镜像 digest，但 `apt-get update && apt-get install` 和 `cargo build` 过程中拉取的包版本可能因时间差异而不同（即使只差几小时，apt 源镜像可能已更新）。

**解决方向**：
1. 将 `apt-get` 替换为固定版本安装，或使用预构建的 builder 镜像
2. 使用 `cargo install --locked` 并确保 Cargo registry index 的确定性
3. 或者：直接分发预构建的 EIF 文件，验证者只需比对 PCR0 而非重建

## 结论

- **计算结果可复现**：相同输入 → 相同输出 → 相同 user_data hash ✅
- **镜像 PCR0 暂不可复现**：需要进一步固定构建环境中的所有外部依赖 ⚠️
- **验证链路完整**：两次构建的 proof 都通过了签名链 + hash 完整性验证 ✅
