use crate::FileEntry;

pub fn generate_html(entries: &[FileEntry], current_path: &str) -> String {
    let entries_json = serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_string());
    let current_path_display = if current_path.is_empty() {
        "/"
    } else {
        current_path
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>😊 Swizzer's Sharing Service - {}</title>
    <link href="https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600&display=swap" rel="stylesheet">
    <link href="https://fonts.googleapis.com/icon?family=Material+Icons" rel="stylesheet">
    <style>
        * {{
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }}
        
        body {{
            font-family: 'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            min-height: 100vh;
            color: #333;
            overflow-x: hidden;
        }}
        
        .container {{
            max-width: 1200px;
            margin: 0 auto;
            padding: 2rem;
        }}
        
        .header {{
            background: rgba(255, 255, 255, 0.95);
            backdrop-filter: blur(20px);
            border-radius: 16px;
            padding: 2rem;
            margin-bottom: 2rem;
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.1);
            border: 1px solid rgba(255, 255, 255, 0.2);
        }}
        
        .header h1 {{
            font-size: 2.5rem;
            font-weight: 600;
            background: linear-gradient(135deg, #667eea, #764ba2);
            -webkit-background-clip: text;
            -webkit-text-fill-color: transparent;
            margin-bottom: 0.5rem;
        }}
        
        .breadcrumb {{
            display: flex;
            align-items: center;
            font-size: 1rem;
            color: #666;
            gap: 0.5rem;
        }}
        
        .breadcrumb .material-icons {{
            font-size: 1.2rem;
            color: #888;
        }}
        
        .file-grid {{
            background: rgba(255, 255, 255, 0.95);
            backdrop-filter: blur(20px);
            border-radius: 16px;
            padding: 2rem;
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.1);
            border: 1px solid rgba(255, 255, 255, 0.2);
        }}
        
        .file-list {{
            display: grid;
            gap: 0.5rem;
        }}
        
        .file-item {{
            display: flex;
            align-items: center;
            padding: 1rem 1.5rem;
            border-radius: 12px;
            text-decoration: none;
            color: inherit;
            transition: all 0.3s cubic-bezier(0.4, 0, 0.2, 1);
            border: 1px solid transparent;
            background: rgba(255, 255, 255, 0.7);
            position: relative;
            overflow: hidden;
        }}
        
        .file-item::before {{
            content: '';
            position: absolute;
            top: 0;
            left: -100%;
            width: 100%;
            height: 100%;
            background: linear-gradient(90deg, transparent, rgba(255, 255, 255, 0.4), transparent);
            transition: left 0.5s;
        }}
        
        .file-item:hover {{
            transform: translateY(-2px);
            box-shadow: 0 8px 25px rgba(0, 0, 0, 0.15);
            border-color: rgba(102, 126, 234, 0.3);
            background: rgba(255, 255, 255, 0.9);
        }}
        
        .file-item:hover::before {{
            left: 100%;
        }}
        
        .file-icon {{
            margin-right: 1rem;
            font-size: 1.5rem;
            width: 2rem;
            text-align: center;
            color: #667eea;
        }}
        
        .file-info {{
            flex: 1;
            display: flex;
            justify-content: space-between;
            align-items: center;
        }}
        
        .file-name {{
            font-weight: 500;
            font-size: 1rem;
            color: #333;
        }}
        
        .file-size {{
            font-size: 0.875rem;
            color: #888;
            font-weight: 400;
        }}
        
        .download-btn {{
            margin-left: 1rem;
            padding: 0.5rem;
            border: none;
            background: linear-gradient(135deg, #667eea, #764ba2);
            color: white;
            border-radius: 8px;
            cursor: pointer;
            transition: all 0.3s ease;
            opacity: 0;
            transform: translateX(10px);
        }}
        
        .file-item:hover .download-btn {{
            opacity: 1;
            transform: translateX(0);
        }}
        
        .download-btn:hover {{
            transform: scale(1.1);
            box-shadow: 0 4px 15px rgba(102, 126, 234, 0.4);
        }}
        
        .download-btn .material-icons {{
            font-size: 1.2rem;
        }}
        
        .empty-state {{
            text-align: center;
            padding: 4rem 2rem;
            color: #888;
        }}
        
        .empty-state .material-icons {{
            font-size: 4rem;
            margin-bottom: 1rem;
            opacity: 0.5;
        }}
        
        @media (max-width: 768px) {{
            .container {{
                padding: 1rem;
            }}
            
            .header {{
                padding: 1.5rem;
            }}
            
            .header h1 {{
                font-size: 2rem;
            }}
            
            .file-grid {{
                padding: 1rem;
            }}
            
            .file-item {{
                padding: 1rem;
            }}
            
            .file-info {{
                flex-direction: column;
                align-items: flex-start;
                gap: 0.5rem;
            }}
            
            .download-btn {{
                position: absolute;
                right: 1rem;
                top: 50%;
                transform: translateY(-50%);
                opacity: 1;
            }}
        }}
        
        .parent-dir {{
            background: linear-gradient(135deg, rgba(102, 126, 234, 0.1), rgba(118, 75, 162, 0.1));
            border: 1px solid rgba(102, 126, 234, 0.2);
        }}
        
        .parent-dir .file-icon {{
            color: #764ba2;
        }}
        
        .fade-in {{
            animation: fadeIn 0.6s ease-out;
        }}
        
        @keyframes fadeIn {{
            from {{
                opacity: 0;
                transform: translateY(20px);
            }}
            to {{
                opacity: 1;
                transform: translateY(0);
            }}
        }}
    </style>
</head>
<body>
    <div class="container">
        <div class="header fade-in">
            <h1>Swizzer's Sharing Service</h1>
            <div class="breadcrumb">
                <span class="material-icons">folder</span>
                <span id="currentPath">{}</span>
            </div>
        </div>
        
        <div class="file-grid fade-in">
            <div class="file-list" id="fileList">
                <!-- 文件列表将通过JavaScript生成 -->
            </div>
        </div>
    </div>
    
    <script>
        const entries = {};
        
        function formatFileSize(bytes) {{
            if (bytes === null || bytes === undefined) return '';
            const sizes = ['B', 'KB', 'MB', 'GB'];
            if (bytes === 0) return '0 B';
            const i = Math.floor(Math.log(bytes) / Math.log(1024));
            return Math.round(bytes / Math.pow(1024, i) * 100) / 100 + ' ' + sizes[i];
        }}
        
        function getFileIcon(fileName, isDir) {{
            if (fileName === '..') return 'keyboard_arrow_up';
            if (isDir) return 'folder';
            
            const ext = fileName.split('.').pop().toLowerCase();
            const iconMap = {{
                'pdf': 'picture_as_pdf',
                'doc': 'description',
                'docx': 'description',
                'xls': 'table_chart',
                'xlsx': 'table_chart',
                'ppt': 'slideshow',
                'pptx': 'slideshow',
                'txt': 'text_snippet',
                'md': 'text_snippet',
                'zip': 'archive',
                'rar': 'archive',
                '7z': 'archive',
                'jpg': 'image',
                'jpeg': 'image',
                'png': 'image',
                'gif': 'image',
                'svg': 'image',
                'mp4': 'movie',
                'avi': 'movie',
                'mkv': 'movie',
                'mp3': 'audiotrack',
                'wav': 'audiotrack',
                'flac': 'audiotrack',
                'js': 'code',
                'html': 'code',
                'css': 'code',
                'json': 'code',
                'xml': 'code',
                'py': 'code',
                'java': 'code',
                'cpp': 'code',
                'c': 'code',
                'rs': 'code'
            }};
            
            return iconMap[ext] || 'insert_drive_file';
        }}
        
        function renderFileList() {{
            const fileList = document.getElementById('fileList');
            
            if (entries.length === 0) {{
                fileList.innerHTML = `
                    <div class="empty-state">
                        <div class="material-icons">folder_open</div>
                        <p>此目录为空</p>
                    </div>
                `;
                return;
            }}
            
            fileList.innerHTML = entries.map((entry, index) => {{
                const icon = getFileIcon(entry.name, entry.is_dir);
                const sizeDisplay = entry.is_dir ? '' : formatFileSize(entry.size);
                const isParentDir = entry.name === '..';
                const itemClass = isParentDir ? 'file-item parent-dir' : 'file-item';
                
                const downloadBtn = !entry.is_dir ? `
                    <button class="download-btn" onclick="downloadFile('${{entry.url}}', event)" title="下载文件">
                        <span class="material-icons">download</span>
                    </button>
                ` : '';
                
                return `
                    <a href="${{entry.url}}" class="${{itemClass}}" style="animation-delay: ${{index * 0.1}}s">
                        <span class="material-icons file-icon">${{icon}}</span>
                        <div class="file-info">
                            <span class="file-name">${{entry.name}}</span>
                            <span class="file-size">${{sizeDisplay}}</span>
                        </div>
                        ${{downloadBtn}}
                    </a>
                `;
            }}).join('');
        }}
        
        function downloadFile(url, event) {{
            event.preventDefault();
            event.stopPropagation();
            window.location.href = url + '?download=1';
        }}
        
        document.addEventListener('DOMContentLoaded', () => {{
            renderFileList();
        }});
    </script>
</body>
</html>"#,
        current_path_display, current_path_display, entries_json
    )
}
