# tg-arceos-tutorial 实验报告

## 1. 实验背景与目标

本仓库 `tg-arceos-tutorial` 是一个聚合型教学仓库，根目录本身是一个 bundle crate，实际实验内容分布在多个独立的 `exercise-*` 与 `app-*` 子 crate 中。本次任务要求基于当前工作目录中的源码，完成以下 5 个实验：

- `exercise-printcolor`
- `exercise-hashmap`
- `exercise-altalloc`
- `exercise-sysmap`
- `exercise-ramfs-rename`

执行原则是“最小必要修改 + 可验证交付”。因此本次工作重点放在：

1. 先理解每个 exercise 的独立构建方式与依赖关系。
2. 尽量把修改限制在题目要求的最小范围内。
3. 每一题都通过 `docker exec` 在容器内进行真实验证。
4. 最后再做一次整体回归，确认公共改动没有引入连带问题。

## 2. 项目结构与任务理解

### 2.1 仓库整体结构

根目录包含：

- `README.md`：说明该仓库是多个教学 crate 的打包集合。
- `scripts/`：提供批量执行脚本，例如 `batch_exercise_exec.sh`。
- `app-*`：教学示例。
- `exercise-*`：本次实验的 5 个目标。

每个 `exercise-*` 基本都采用相同结构：

- `src/main.rs`：实验应用入口。
- `xtask/src/main.rs`：构建与 QEMU 运行入口。
- `configs/*.toml`：不同架构配置。
- `scripts/test.sh`：官方验证脚本。
- `Cargo.toml`：独立 crate 配置。

### 2.2 构建与运行方式

仓库约定是进入某个 `exercise-*` 目录后，用：

```bash
cargo xtask run --arch=<arch>
```

进行构建和运行。  
本次根据要求，所有编译、运行、测试行为都放在 Docker 容器中执行。

### 2.3 Docker 环境与挂载路径

实际使用的测试容器为：

- 容器名：`tg-arceos-tutorial-ci`
- 仓库挂载路径：`/workspace/tg-arceos-tutorial`

确认命令：

```bash
docker inspect tg-arceos-tutorial-ci --format '{{json .Mounts}}'
docker exec tg-arceos-tutorial-ci sh -lc 'pwd'
```

### 2.4 exercise 与公共模块关系

5 个实验中，依赖关系分为两类：

- 仅修改应用层即可完成：
  - `exercise-printcolor`
- 需要 patch 公共 crate 或本地替换上游组件：
  - `exercise-hashmap`：本地 patch `axstd`
  - `exercise-altalloc`：补全本地 `bump_allocator`
  - `exercise-sysmap`：在 syscall 层补 Linux ABI 兼容
  - `exercise-ramfs-rename`：本地 patch `axfs` 和 `axfs_ramfs`

## 3. 五个 exercise 的实现思路

### 3.1 exercise-printcolor

#### 目标

输出 `Hello, Arceos!`，并且串口输出中带有 ANSI 颜色控制序列。

#### 关键代码

- `exercise-printcolor/src/main.rs`

#### 实现

不修改公共库，直接在应用输出中加入 ANSI SGR 颜色序列：

```rust
println!("\x1b[1;32m[WithColor]: Hello, Arceos!\x1b[0m");
```

这是满足题意的最小修改。

---

### 3.2 exercise-hashmap

#### 目标

让 `axstd` 支持 `std::collections::HashMap`，使测试程序能够成功编译并运行内存测试。

#### 关键代码

- `exercise-hashmap/Cargo.toml`
- `exercise-hashmap/axstd/src/lib.rs`
- `exercise-hashmap/axstd/src/collections.rs`

#### 实现

做法是把 `axstd 0.3.0-preview.1` 的源码放到本地，并通过 `[patch.crates-io]` 覆盖 crates.io 版本。  
在本地 `axstd` 中新增 `collections` 模块：

- 继续导出 `alloc::collections` 原有的 `BTreeMap`、`VecDeque` 等。
- 用 `hashbrown` 提供 `HashMap` 和 `HashSet`。

这样应用中的：

```rust
use std::collections::HashMap;
```

就能正常解析到本地 `axstd::collections::HashMap`。

---

### 3.3 exercise-altalloc

#### 目标

实现一个 bump 风格分配器，同时实现：

- `BaseAllocator`
- `ByteAllocator`
- `PageAllocator`

#### 关键代码

- `exercise-altalloc/modules/bump_allocator/src/lib.rs`

#### 实现

为 `EarlyAllocator` 增加以下状态：

- `start`：可管理内存起点
- `end`：可管理内存终点
- `b_pos`：字节分配向前增长的位置
- `p_pos`：页分配向后增长的位置
- `count`：字节分配计数

实现策略：

- 字节分配从低地址向高地址 bump。
- 页分配从高地址向低地址 bump。
- 两端相遇时返回 `NoMemory`。
- 字节分配只在 `count` 降为 0 时整体复位，不做细粒度回收。
- 页分配不回收，符合题目中 early/bump allocator 的预期。

---

### 3.4 exercise-sysmap

#### 目标

实现 `SYS_MMAP`，使 `/sbin/mapfile` 用户程序能够通过 `mmap` 把文件映射到用户地址空间并读出内容。

#### 关键代码

- `exercise-sysmap/src/syscall.rs`
- `exercise-sysmap/xtask/src/main.rs`

#### 实现

##### 1. 实现 `SYS_MMAP`

核心流程：

1. 解析 `prot` 和 `flags`
2. 在 `USER_ASPACE` 中找到可用虚拟地址区间
3. 用 `map_alloc` 建立用户页映射
4. 如果是文件映射，则通过 fd 对应文件 `read_at(offset, ...)`
5. 把读出的内容写入新映射区域
6. 返回映射地址

##### 2. 修复 `SYS_BRK`

在实际验证中发现，最初 `brk` 只更新了程序断点，没有映射用户堆页，导致用户态一旦写堆就页故障。  
因此补充了：

- `PROGRAM_BRK_MAPPED`
- 当新的 brk 超过当前已映射边界时，自动在 `USER_ASPACE` 中补齐堆映射

##### 3. 补最小 Linux ABI 兼容

由于容器内没有 `riscv64-linux-musl-gcc`，实际验证改用了 `riscv64-linux-gnu-gcc -static` 路径。  
这使得用户程序启动阶段比 musl 多依赖若干 syscall。为保证实验程序能继续执行，补充了最小兼容实现：

- 身份相关：`getuid/geteuid/getgid/getegid/getpid/gettid`
- 运行时初始化：`uname/set_robust_list`
- 时间与随机：`clock_gettime/getrandom`
- 信号相关：`rt_sigaction/rt_sigprocmask/tgkill`
- 内存保护：`mprotect`
- 资源限制：`prlimit64`
- 路径辅助：`readlinkat`

这些实现都保持了“仅满足本实验需要”的最小语义。

##### 4. 调整 xtask 的工具链查找逻辑

在 `exercise-sysmap/xtask/src/main.rs` 中把交叉编译器查找从：

- 仅尝试 `*-linux-musl-*`

改成：

- 先尝试 `*-linux-musl-*`
- 找不到时回退到 `*-linux-gnu-*`

这样在容器没有 musl 工具链时仍能完成验证。

---

### 3.5 exercise-ramfs-rename

#### 目标

让 `std::fs::rename` 在 ramfs 根文件系统路径上正常工作。

#### 关键代码

- `exercise-ramfs-rename/Cargo.toml`
- `exercise-ramfs-rename/axfs/src/root.rs`
- `exercise-ramfs-rename/axfs_ramfs/src/dir.rs`

#### 实现

##### 1. 本地 patch 相关文件系统 crate

通过：

```toml
[patch.crates-io]
axfs = { path = "./axfs" }
axfs_ramfs = { path = "./axfs_ramfs" }
```

让 exercise 使用本地修改后的 `axfs` 和 `axfs_ramfs`。

##### 2. 在 `axfs::RootDirectory` 上补转发

`std::fs::rename` 最终会走到 `axfs::root::rename`，再交给根目录节点。  
因此补了 `RootDirectory::rename`：

- 若源路径和目标路径都在同一个 mount 上，则转发到对应文件系统根节点
- 若跨 mount，则返回错误

##### 3. 在 `axfs_ramfs::DirNode` 上实现 rename

在 ramfs 目录节点里实现：

- 解析源路径与目标路径的父目录
- 要求源与目标在同一父目录
- 从 `children` 中移除旧名字，再插入新名字

该实现严格符合题目里“只支持 rename，不支持 move”的说明。

## 4. 关键代码修改说明

本次最终修改涉及：

- `exercise-printcolor/src/main.rs`
- `exercise-hashmap/Cargo.toml`
- `exercise-hashmap/axstd/*`
- `exercise-altalloc/modules/bump_allocator/src/lib.rs`
- `exercise-sysmap/src/syscall.rs`
- `exercise-sysmap/xtask/src/main.rs`
- `exercise-ramfs-rename/Cargo.toml`
- `exercise-ramfs-rename/axfs/*`
- `exercise-ramfs-rename/axfs_ramfs/*`

其中：

- `printcolor` 是应用层单点修改。
- `hashmap` 和 `ramfs-rename` 都采用了本地 patch crates 的方式。
- `altalloc` 集中在一个分配器文件中完成。
- `sysmap` 的主要复杂度来自验证过程中逐步暴露的 syscall 兼容问题。

## 5. 调试过程与报错分析

### 5.1 Docker 与磁盘空间问题

实验初期容器内编译产生大量 `target/` 目录，导致宿主机磁盘空间不足，Docker 一度出现 I/O error。  
空间清理后重新恢复了 Docker，并继续用同一个挂载仓库进行验证。

### 5.2 exercise-hashmap 初始编译失败

初始错误为：

```text
unresolved import `std::collections::HashMap`
```

说明 crates.io 上的 `axstd` 版本并未提供 `HashMap`。  
解决方式是 patch 本地 `axstd` 并导出 `hashbrown::HashMap`。

### 5.3 exercise-sysmap 的多轮问题

`sysmap` 是本次调试最复杂的一题，问题是逐层暴露出来的：

1. 容器里缺少 `riscv64-linux-musl-gcc`
2. 改为 `gnu` fallback 后，用户态启动需要更多基础 syscall
3. `brk` 没有真实映射堆页，导致页故障
4. `mprotect` 未实现，导致运行时打印：

```text
cannot apply additional memory protection after relocation
```

逐步补齐之后，最终才让 `MapFile` 测试程序完整跑通。

### 5.4 exercise-ramfs-rename 编译期问题

第一次编译本地 `axfs_ramfs` 时，`parent_dir_of` 的返回生命周期没有显式绑定到输入 `path`，触发生命周期错误。  
显式写出：

```rust
fn parent_dir_of<'a>(&self, path: &'a str) -> VfsResult<(VfsNodeRef, &'a str)>
```

后解决。

## 6. 验证方法与结果

### 6.1 单题验证命令

#### printcolor

```bash
docker exec tg-arceos-tutorial-ci sh -lc 'cd /workspace/tg-arceos-tutorial/exercise-printcolor && bash scripts/test.sh'
```

结果：

- `riscv64` 通过
- 输出中检测到 ANSI 颜色序列

#### hashmap

```bash
docker exec tg-arceos-tutorial-ci sh -lc 'cd /workspace/tg-arceos-tutorial/exercise-hashmap && bash scripts/test.sh'
```

结果：

- `riscv64` 通过
- 输出匹配：
  - `test_hashmap() OK!`
  - `Memory tests run OK!`

#### altalloc

```bash
docker exec tg-arceos-tutorial-ci sh -lc 'cd /workspace/tg-arceos-tutorial/exercise-altalloc && bash scripts/test.sh'
```

结果：

- `riscv64` 通过
- 输出匹配：
  - `Running bump tests...`
  - `Bump tests run OK!`

#### sysmap

```bash
docker exec tg-arceos-tutorial-ci sh -lc 'cd /workspace/tg-arceos-tutorial/exercise-sysmap && bash scripts/test.sh'
```

结果：

- `riscv64` 通过
- 输出匹配：
  - `Read back content: hello, arceos!`
  - `MapFile ok!`

#### ramfs-rename

```bash
docker exec tg-arceos-tutorial-ci sh -lc 'cd /workspace/tg-arceos-tutorial/exercise-ramfs-rename && bash scripts/test.sh'
```

结果：

- `riscv64` 通过
- 输出匹配：
  - `[Ramfs-Rename]: ok!`

### 6.2 整体回归命令

```bash
docker exec tg-arceos-tutorial-ci sh -lc 'cd /workspace/tg-arceos-tutorial && ./scripts/batch_exercise_exec.sh -c "bash scripts/test.sh"'
```

### 6.3 整体回归结果

批量回归结果为：

- 总计：5 个目录
- 成功：5 个
- 失败：0 个

说明本次局部 patch 没有引入 exercise 之间的连带回归。

### 6.4 架构覆盖说明

当前容器中仅提供 `qemu-system-riscv64`。因此所有官方脚本都实际执行了 `riscv64`，其余：

- `x86_64`
- `aarch64`
- `loongarch64`

都被脚本识别为缺少 QEMU 而自动跳过。  
因此本次“真实执行验证”覆盖的是 `riscv64` 路径。

## 7. 总结与收获

本次实验最终完成了 5 个 exercise，且单题验证与整体回归均通过。

主要收获包括：

1. 理解了 ArceOS 教学仓库中独立 exercise crate 的组织方式，以及 `xtask + QEMU` 的统一构建/运行约定。
2. 掌握了通过 `[patch.crates-io]` 在题目目录内最小替换公共 crate 的方式。
3. 理解了 bump allocator 的双端分配模型，以及早期分配器与常规 allocator 的差异。
4. 在 `sysmap` 中实际经历了从单一 `mmap` 缺失，逐步扩展到用户态 ABI 兼容问题的调试过程。
5. 理解了 ramfs 根目录、挂载点转发以及 VFS rename 路径在 ArceOS 中的组织方式。

最终交付包括：

- 已修改完成的 5 个 exercise 代码
- 单题验证结果
- 整体回归结果
- 本报告 `report.md`

