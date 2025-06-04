# HTTP 文件服务器

用Rust编写的简易HTTP文件服务器，类似于Python的`python -m http.server`


## Usage

- `--bind` 参数指定绑定IP地址
- `--port` 参数指定绑定端口
- 命令行参数指定工作目录

## Example

- `cargo run -- --bind 0.0.0.0 --port 3000 /path/to/files`
- `[compiled_exec_file] --bind 127.0.0.1 --port 3000 /path/to/files`

## TODO
- [x] ~~更美观的前端页面~~ -> 可点击的路径