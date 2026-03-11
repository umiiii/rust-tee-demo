# Vsock 吞吐量基准测试报告

## 测试环境

| 项目 | 配置 |
|------|------|
| 实例类型 | AWS c8i.2xlarge |
| 操作系统 | Amazon Linux 2023 (kernel 6.1.92) |
| CPU | 6 vCPU（其中 2 vCPU 分配给 Enclave） |
| 内存 | 16 GiB（其中 256 MiB 分配给 Enclave） |
| Nitro CLI | 1.3.0 |
| Rust | 1.82.0 (Enclave 构建) / 1.93.1 (Host 构建) |

## 测试方案

通过 vsock（端口 5001）在 EC2 Host 与 Nitro Enclave 之间传输零填充数据，测量原始吞吐量。

协议设计：Host 先发送 24 字节控制消息（总字节数 + 缓冲区大小 + 标志位），随后按指定缓冲区大小分块发送数据，Enclave 接收完毕后回传 8 字节确认。若标志位设置了反向测试，则 Enclave 再向 Host 发送同等数据量。

## 核心代码

### Enclave 端（接收 + 发送）

```rust
fn receive_data(stream: &mut UnixStream, total_bytes: u64, buffer_size: u64) -> io::Result<u64> {
    let mut buf = vec![0u8; buffer_size as usize];
    let mut received: u64 = 0;
    while received < total_bytes {
        let to_read = ((total_bytes - received) as usize).min(buf.len());
        let n = stream.read(&mut buf[..to_read])?;
        if n == 0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")); }
        received += n as u64;
    }
    Ok(received)
}

fn send_data(stream: &mut UnixStream, total_bytes: u64, buffer_size: u64) -> io::Result<()> {
    let buf = vec![0u8; buffer_size as usize];
    let mut sent: u64 = 0;
    while sent < total_bytes {
        let to_write = ((total_bytes - sent) as usize).min(buf.len());
        stream.write_all(&buf[..to_write])?;
        sent += to_write as u64;
    }
    Ok(())
}
```

### Host 端（计时 + 发送数据）

```rust
// 发送控制消息 (24 bytes: total_bytes + buffer_size + flags, 均为 u64 BE)
let mut ctrl = [0u8; 24];
ctrl[0..8].copy_from_slice(&total_bytes.to_be_bytes());
ctrl[8..16].copy_from_slice(&buffer_size.to_be_bytes());
ctrl[16..24].copy_from_slice(&flags.to_be_bytes());
stream.write_all(&ctrl)?;

// Phase 1: Host -> Enclave（计时包含发送 + 等待 ack）
let start = Instant::now();
send_data(&mut stream, total_bytes, buffer_size)?;
let mut ack = [0u8; 8];
stream.read_exact(&mut ack)?;
let duration = start.elapsed();

// Phase 2: Enclave -> Host（如设置反向标志）
let start = Instant::now();
receive_data(&mut stream, total_bytes, buffer_size)?;
stream.write_all(&received.to_be_bytes())?;
let duration = start.elapsed();
```

## 测试结果

### 一、不同数据量测试（缓冲区 = 64 KiB，Host -> Enclave）

| 数据量 | 耗时 (s) | 吞吐量 (MB/s) | 吞吐量 (MiB/s) |
|--------|----------|---------------|----------------|
| 1 GiB  | 1.038    | 1,034         | 986            |
| 2 GiB  | 2.018    | 1,064         | 1,015          |
| 3 GiB  | 3.083    | 1,045         | 997            |
| 4 GiB  | 4.141    | 1,037         | 989            |
| 5 GiB  | 5.140    | 1,044         | 996            |
| 6 GiB  | 6.177    | 1,043         | 995            |
| 7 GiB  | 7.127    | 1,055         | 1,006          |
| 8 GiB  | 8.156    | 1,053         | 1,004          |
| 9 GiB  | 9.219    | 1,048         | 1,000          |
| 10 GiB | 10.295   | 1,043         | 995            |

### 二、不同缓冲区大小测试（数据量 = 256 MiB，双向）

| 方向 | 缓冲区 | 耗时 (s) | 吞吐量 (MB/s) |
|------|--------|----------|---------------|
| Host -> Enclave | 4 KiB   | 0.475 | 565   |
| Enclave -> Host | 4 KiB   | 0.387 | 694   |
| Host -> Enclave | 64 KiB  | 0.240 | 1,117 |
| Enclave -> Host | 64 KiB  | 0.381 | 704   |
| Host -> Enclave | 256 KiB | 0.250 | 1,075 |
| Enclave -> Host | 256 KiB | 0.382 | 704   |

### 三、双向测试（数据量 = 1 GiB，缓冲区 = 64 KiB）

| 方向 | 耗时 (s) | 吞吐量 (MB/s) | 吞吐量 (MiB/s) |
|------|----------|---------------|----------------|
| Host -> Enclave | 1.026 | 1,047 | 998  |
| Enclave -> Host | 1.529 | 702   | 670  |

## 总结

1. **Host -> Enclave 吞吐量稳定在 ~1,045 MB/s（~1 GB/s）**，从 1 GiB 到 10 GiB 线性扩展，无性能衰减。

2. **Enclave -> Host 吞吐量约 700 MB/s**，低于反方向约 33%，且不随缓冲区大小变化，推测受 vsock 实现中反向路径的流控机制限制。

3. **缓冲区大小对 Host -> Enclave 方向影响显著**：4 KiB 时仅 565 MB/s，64 KiB 时达到 1,117 MB/s，之后增大缓冲区收益递减。建议生产使用时缓冲区不小于 64 KiB。

4. **实际应用参考**：以典型的机器学习推理场景为例，传入一个 100 MB 的模型输入数据到 Enclave 仅需约 0.1 秒，vsock 传输不会成为瓶颈。
