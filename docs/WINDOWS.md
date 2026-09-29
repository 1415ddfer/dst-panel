# Windows 部署

1. 从 GitHub Release 下载 `dst-panel.<版本>-windows.zip`，解压到有写入权限的目录。
2. 在该目录打开 PowerShell，运行 `./dst-admin-rust.exe`。程序会在当前目录读取 `config.yml`，并写入 `data`、`.klei` 和日志文件。
3. 浏览器打开 `http://127.0.0.1:8082`（若修改了 `config.yml` 的 `port`，使用修改后的端口），完成首次初始化。
4. 在面板中安装 SteamCMD 和 DST。SteamCMD 位于解压目录下的 `steamcmd`，服务器文件位于 `dst-dedicated-server`，世界数据位于 `.klei/DoNotStarveTogether`。

Windows 版直接运行 `.exe`，无需 Docker、`screen` 或 Linux shell。首次安装需要能够访问 Steam 的下载服务。若已有旧版数据，请在替换程序前备份 `data`、`.klei`、`dst_config` 和 `password.txt`。
