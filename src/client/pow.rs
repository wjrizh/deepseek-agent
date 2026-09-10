//! PoW 求解：DeepSeekHashV1。
//! 直接加载前端原始 `sha3.wasm`，调 `wasm_solve` 计算。
//!
//! 采用 TypedFunc（编译期签名检查，性能优于通用 Func + Val），
//! 内存直接 read/write，完全绕开 Val 枚举。

use crate::error::{AgentError, Result};
use wasmi::{Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

type SolveFn = TypedFunc<(i32, i32, i32, i32, i32, f64), ()>;
type MallocFn = TypedFunc<(i32, i32), i32>;
type StackFn = TypedFunc<i32, i32>;

pub struct PowSolver {
    store: Store<()>,
    memory: Memory,
    solve: SolveFn,
    malloc: MallocFn,
    stack: StackFn,
}

impl PowSolver {
    /// 从 wasm 字节创建求解器
    pub fn new(wasm_bytes: &[u8]) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::new(&engine, wasm_bytes)
            .map_err(|e| AgentError::Wasm(format!("模块加载失败: {e}")))?;
        let mut store = Store::new(&engine, ());
        let linker: Linker<()> = Linker::new(&engine);
        let instance: Instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| AgentError::Wasm(format!("实例化失败: {e}")))?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| AgentError::Wasm("未找到 memory".into()))?;
        let solve = instance
            .get_typed_func::<(i32, i32, i32, i32, i32, f64), ()>(&store, "wasm_solve")
            .map_err(|e| AgentError::Wasm(format!("wasm_solve 签名不匹配: {e}")))?;
        let malloc = instance
            .get_typed_func::<(i32, i32), i32>(&store, "__wbindgen_export_0")
            .map_err(|e| AgentError::Wasm(format!("malloc 签名不匹配: {e}")))?;
        let stack = instance
            .get_typed_func::<i32, i32>(&store, "__wbindgen_add_to_stack_pointer")
            .map_err(|e| AgentError::Wasm(format!("stack 签名不匹配: {e}")))?;

        Ok(Self {
            store,
            memory,
            solve,
            malloc,
            stack,
        })
    }

    /// 从文件创建
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::new(&bytes)
    }

    /// 计算答案。返回 None 表示无解。
    pub fn solve(&mut self, challenge: &str, prefix: &str, difficulty: i32) -> Result<Option<f64>> {
        // 1. 栈上开 16 字节输出区
        let stack = self
            .stack
            .call(&mut self.store, -16)
            .map_err(|e| AgentError::Wasm(format!("stack 操作失败: {e}")))?;

        // 2. 写入字符串
        let ch_ptr = self.write_string(challenge)?;
        let pfx_ptr = self.write_string(prefix)?;

        // 3. 调用 wasm_solve
        self.solve
            .call(
                &mut self.store,
                (
                    stack,
                    ch_ptr as i32,
                    challenge.len() as i32,
                    pfx_ptr as i32,
                    prefix.len() as i32,
                    difficulty as f64,
                ),
            )
            .map_err(|e| AgentError::Pow(format!("wasm_solve 调用失败: {e}")))?;

        // 4. 读结果
        let status = self.read_i32(stack)?;
        let answer = self.read_f64(stack + 8)?;

        // 5. 恢复栈
        let _ = self.stack.call(&mut self.store, 16);

        if status == 0 {
            Ok(None)
        } else {
            Ok(Some(answer))
        }
    }

    /// DeepSeekHashV1：prefix = salt + "_" + expire_at + "_"
    pub fn solve_challenge(
        &mut self,
        challenge: &str,
        salt: &str,
        expire_at: i64,
        difficulty: i32,
    ) -> Result<f64> {
        let prefix = format!("{salt}_{expire_at}_");
        self.solve(challenge, &prefix, difficulty)?
            .ok_or_else(|| AgentError::Pow("PoW 无解".into()))
    }

    // ---------- 内部辅助 ----------

    fn write_string(&mut self, s: &str) -> Result<u32> {
        let bytes = s.as_bytes();
        let ptr = self
            .malloc
            .call(&mut self.store, (bytes.len() as i32, 1))
            .map_err(|e| AgentError::Wasm(format!("malloc 失败: {e}")))? as u32;
        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .map_err(|e| AgentError::Wasm(format!("内存写入失败: {e}")))?;
        Ok(ptr)
    }

    fn read_i32(&self, offset: i32) -> Result<i32> {
        let mut buf = [0u8; 4];
        self.memory
            .read(&self.store, offset as usize, &mut buf)
            .map_err(|e| AgentError::Wasm(format!("内存读取失败: {e}")))?;
        Ok(i32::from_le_bytes(buf))
    }

    fn read_f64(&self, offset: i32) -> Result<f64> {
        let mut buf = [0u8; 8];
        self.memory
            .read(&self.store, offset as usize, &mut buf)
            .map_err(|e| AgentError::Wasm(format!("内存读取失败: {e}")))?;
        Ok(f64::from_le_bytes(buf))
    }
}
