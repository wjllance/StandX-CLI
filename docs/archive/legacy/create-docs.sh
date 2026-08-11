#!/bin/bash
# create-docs.sh - 批量创建文档的辅助脚本
# ARCHIVED: this early helper only describes docs 01-11 and no longer reflects
# the repository documentation layout. Keep it for history; do not use it to
# generate or validate the current docs index.

echo "StandX CLI 文档目录已创建"
echo ""
echo "文档结构:"
echo "docs/"
echo "├── README.md                 # 文档目录说明"
echo "├── 01-getting-started.md     # 快速开始"
echo "├── 02-authentication.md      # 认证管理"
echo "├── 03-market-data.md         # 市场数据"
echo "├── 04-account.md             # 账户信息"
echo "├── 05-orders.md              # 订单管理"
echo "├── 06-trading.md             # 交易历史"
echo "├── 07-leverage-margin.md     # 杠杆与保证金"
echo "├── 08-streaming.md           # 实时数据流"
echo "├── 09-output-formats.md      # 输出格式"
echo "├── 10-special-features.md    # 特殊功能"
echo "└── 11-troubleshooting.md     # 故障排除"
echo ""
echo "已创建文档:"
ls -la docs/
