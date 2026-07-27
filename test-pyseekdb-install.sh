#!/usr/bin/env bash
# 测试 pyseekdb 安装各步骤耗时
# 用法: bash test-pyseekdb-install.sh [--keep]
#   --keep  保留临时目录，默认测试完自动清理

set -euo pipefail

KEEP="${1:-}"
RUNTIME="$(mktemp -d /tmp/pyseekdb-test-XXXXXX)"
PYTHON_BIN="$RUNTIME/bin/python"
MARKER="$RUNTIME/.pyseekdb-1.4.0"

cleanup() {
  if [ "$KEEP" != "--keep" ]; then
    echo ""
    echo "=== 清理临时目录 ==="
    rm -rf "$RUNTIME"
    echo "已删除: $RUNTIME"
  else
    echo ""
    echo "=== 保留临时目录 ==="
    echo "路径: $RUNTIME"
  fi
}
trap cleanup EXIT

echo "============================================"
echo "  pyseekdb 安装耗时测试"
echo "============================================"
echo ""
echo "临时目录: $RUNTIME"
echo ""

# Step 1: 查找 uv
echo "--- Step 1: 查找 uv ---"
if command -v uv &>/dev/null; then
  UV="uv"
elif [ -f "$HOME/.cargo/bin/uv" ]; then
  UV="$HOME/.cargo/bin/uv"
else
  echo "错误: 找不到 uv，请先安装 uv"
  exit 1
fi
echo "uv 路径: $UV"
echo "uv 版本: $($UV --version 2>&1)"

# Step 2: 创建 Python 3.12 venv
echo ""
echo "--- Step 2: uv venv --python 3.12 ---"
START_VENV=$(python3 -c "import time; print(int(time.time()*1000))")
$UV venv "$RUNTIME" --python 3.12
END_VENV=$(python3 -c "import time; print(int(time.time()*1000))")
DURATION_VENV=$(( (END_VENV - START_VENV) / 1000 ))
echo "Python 路径: $PYTHON_BIN"
echo "Python 版本: $($PYTHON_BIN --version 2>&1)"
echo "耗时: ${DURATION_VENV}s"

# Step 3: uv pip install pyseekdb==1.4.0
echo ""
echo "--- Step 3: uv pip install pyseekdb==1.4.0 ---"
START_PIP=$(python3 -c "import time; print(int(time.time()*1000))")
$UV pip install \
  --python "$PYTHON_BIN" \
  "pyseekdb==1.4.0"
END_PIP=$(python3 -c "import time; print(int(time.time()*1000))")
DURATION_PIP=$(( (END_PIP - START_PIP) / 1000 ))
echo "耗时: ${DURATION_PIP}s"

# 查看安装大小
echo ""
echo "--- Step 4: 安装产物 ---"
echo "runtime 总大小: $(du -sh "$RUNTIME" 2>/dev/null | cut -f1)"
echo "seekdb 二进制: $(find "$RUNTIME" -name seekdb -type f -perm +111 2>/dev/null | head -1)"
echo "seekdb 大小:   $(du -sh "$RUNTIME/lib/python3.12/site-packages/pylibseekdb/seekdb" 2>/dev/null | cut -f1)"
echo "site-packages: $(ls "$RUNTIME/lib/python3.12/site-packages/" 2>/dev/null | tr '\n' ' ')"

# Step 5: 验证导入
echo ""
echo "--- Step 5: 验证 pyseekdb 导入 ---"
START_IMPORT=$(python3 -c "import time; print(int(time.time()*1000))")
$PYTHON_BIN -c "import pyseekdb; print('pyseekdb 版本:', pyseekdb.__version__)"
END_IMPORT=$(python3 -c "import time; print(int(time.time()*1000))")
DURATION_IMPORT=$(( (END_IMPORT - START_IMPORT) / 1000 ))
echo "第一次 import 耗时: ${DURATION_IMPORT}s"


# Step 6: 首次 seekdb 冷启动
echo ""
echo "--- Step 6: 首次 seekdb 冷启动 (模拟 AgentSeek 初始化存储) ---"
SEEKDB_DATA="$(mktemp -d /tmp/seekdb-data-XXXXXX)"
START_BOOT=$(python3 -c "import time; print(int(time.time()*1000))")
$PYTHON_BIN -c "
import time, pyseekdb
t0 = time.time()
admin = pyseekdb.AdminClient(path='$SEEKDB_DATA')
admin.create_database('test_db')
print(f'seekdb bootstrap 耗时: {time.time() - t0:.1f}s')
"
END_BOOT=$(python3 -c "import time; print(int(time.time()*1000))")
DURATION_BOOT=$(( (END_BOOT - START_BOOT) / 1000 ))
echo "耗时: ${DURATION_BOOT}s"
echo "数据目录大小: $(du -sh $SEEKDB_DATA 2>/dev/null | cut -f1)"
if [ "$KEEP" != "--keep" ]; then
  rm -rf "$SEEKDB_DATA"
else
  echo "保留数据目录: $SEEKDB_DATA"
fi

# 汇总
BOOTSTRAP_SEC=${DURATION_BOOT:-0}
TOTAL=$(( DURATION_VENV + DURATION_PIP + DURATION_IMPORT + DURATION_BOOT ))
echo ""
echo "============================================"
echo "  耗时汇总"
echo "============================================"
printf "  uv venv (Python 3.12):     %4ss\n" "$DURATION_VENV"
printf "  uv pip install pyseekdb:   %4ss\n" "$DURATION_PIP"
printf "  import pyseekdb (首次):    %4ss\n" "$DURATION_IMPORT"
printf "  seekdb 首次冷启动:         %4ss\n" "$DURATION_BOOT"
echo "  --------------------------------"
printf "  总计:                       %4ss\n" "$TOTAL"
echo "============================================"
echo ""
echo "磁盘占用: $(du -sh "$RUNTIME" | cut -f1)"
