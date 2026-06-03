#!/usr/bin/env python3
"""Weft 批量修复脚本: 为引用已排除模块的 use 语句添加 #[cfg(not(feature = "weft"))] 包裹。

排除的模块: auth, autoupdate, billing, cloud_object, drive, pricing,
             referral_theme_status, reward_view, server, workspaces
"""

import os
import re
import sys

EXCLUDED_MODULES = [
    "auth", "autoupdate", "billing", "cloud_object", "drive",
    "pricing", "referral_theme_status", "reward_view", "server", "workspaces",
]

ROOT_DIR = "/Users/andylee/McDull/Claude/projects/Weft/app/src"

# 需要跳过的目录 (已在 lib.rs 层面排除)
SKIP_DIRS = {
    "auth", "autoupdate", "billing", "cloud_object", "drive",
    "pricing", "referral_theme_status.rs", "reward_view",
}


def build_patterns():
    """构建所有需要匹配的 use 模式"""
    patterns = []
    for m in EXCLUDED_MODULES:
        # use crate::module::... 或 use crate::module; 或 use crate::module{...}
        patterns.append((re.compile(rf'^(\s*use\s+crate::{m})(::|;|\s|\n|\{{)'), m))
        # use module::... (无 crate 前缀，用于 lib.rs/子模块自身)
        patterns.append((re.compile(rf'^(\s*use\s+{m})(::|;|\s|\n|\{{)'), m))
        # use warp_server_auth::... 等外部 crate
    # 外部 crate 引用
    external_crates = [
        "warp_server_auth", "warp_server_client",
        "cloud_objects", "cloud_object_client", "cloud_object_models", "cloud_object_persistence",
        "onboarding", "session_sharing_protocol",
    ]
    for ec in external_crates:
        patterns.append((re.compile(rf'^(\s*use\s+{ec})(::|;|\s|\n|\{{)'), ec))
    return patterns


def should_skip_dir(dirpath):
    """检查是否应该跳过整个目录"""
    for skip in SKIP_DIRS:
        skip_path = os.path.join(ROOT_DIR, skip)
        if dirpath.startswith(skip_path) or dirpath == skip_path:
            return True
    return False


def line_is_inside_comment_block(lines, idx):
    """检查行是否在块注释 /* */ 内部 (简化版)"""
    in_block = False
    for i in range(idx + 1):
        line = lines[i]
        if "/*" in line and "*/" not in line.split("/*", 1)[1]:
            in_block = True
        if "*/" in line and in_block:
            in_block = False
    return in_block


def already_has_cfg(lines, idx):
    """检查前一行是否已有 #[cfg(...)] 属性"""
    if idx == 0:
        return False
    prev = lines[idx - 1].strip()
    return prev.startswith("#[cfg(")


def process_file(filepath, patterns, dry_run=False):
    """处理单个 Rust 文件"""
    with open(filepath, "r", encoding="utf-8") as f:
        lines = f.readlines()

    modified = False
    new_lines = []
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()

        # 跳过空行和单行注释
        if stripped.startswith("//"):
            new_lines.append(line)
            i += 1
            continue

        # 检查是否匹配排除模块
        matched = False
        for pattern, _ in patterns:
            m = pattern.match(stripped)
            if m:
                # 跳过已有 cfg 属性包裹的情况
                if already_has_cfg(new_lines, len(new_lines)):
                    new_lines.append(line)
                    matched = True
                    break

                # 跳过 pub use warp_server_auth:: 这类 re-export
                if stripped.startswith("pub use ") and stripped.endswith(";"):
                    # pub use 通常需要保留或特殊处理，直接包裹
                    pass

                # 插入 cfg 属性
                indent = line[:len(line) - len(line.lstrip())]
                cfg_line = f'{indent}#[cfg(not(feature = "weft"))]\n'
                new_lines.append(cfg_line)
                new_lines.append(line)
                modified = True
                matched = True
                break

        if not matched:
            new_lines.append(line)
        i += 1

    if modified and not dry_run:
        with open(filepath, "w", encoding="utf-8") as f:
            f.writelines(new_lines)

    return modified


def main():
    dry_run = "--dry-run" in sys.argv
    patterns = build_patterns()

    total_files = 0
    modified_files = 0

    for root, dirs, filenames in os.walk(ROOT_DIR):
        # 跳过排除的目录
        if should_skip_dir(root):
            dirs.clear()
            continue

        # 修改 dirs 列表以跳过不需要遍历的目录
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]

        for filename in filenames:
            if not filename.endswith(".rs"):
                continue

            filepath = os.path.join(root, filename)
            total_files += 1

            if process_file(filepath, patterns, dry_run=dry_run):
                modified_files += 1
                relpath = os.path.relpath(filepath, ROOT_DIR)
                print(f"  [MODIFIED] {relpath}")

    print(f"\n总扫描文件: {total_files}")
    print(f"已修改文件: {modified_files}")
    if dry_run:
        print("(仅预览模式，未实际修改)")
        print("使用 --apply 参数执行实际修改")
    print("完成。")


if __name__ == "__main__":
    main()
