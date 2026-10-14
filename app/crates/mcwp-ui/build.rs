use slint_build::EmbedResourcesKind;

fn main() {
    // 面板 UI 在 `ui/panel.slint`;改完这个文件增量重编即可,不影响壁纸渲染。
    //
    // `EmbedFiles` 让 panel.slint 里 import 的字体数据直接编进 exe,运行时由生成的
    // 初始化代码 `register_font_from_memory()` 注册。面板因此是自包含的:既不用管
    // 系统装没装对应字体,也不用把 ttf 释放到磁盘上再按路径加载。
    //
    // ⚠️ 字体能进来,靠的是 `ui/panel.slint` 顶部那两行 `import "../fonts/*.ttf"`。
    // 删掉它们不会编译报错,只会让 CJK 静默不画(回落系统字体兜底拉丁字母),顺手记一笔。
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/panel.slint", config).expect("编译 Slint 面板失败");

    // 字体是子集化的,改了面板文案就得重跑 tools/subset-panel-font.py,顺手盯着点。
    println!("cargo:rerun-if-changed=ui/panel.slint");
    println!("cargo:rerun-if-changed=fonts/panel-cjk-regular.ttf");
    println!("cargo:rerun-if-changed=fonts/panel-cjk-bold.ttf");
    println!("cargo:rerun-if-changed=fonts/panel-charset.txt");
    println!("cargo:rerun-if-changed=assets/mcwp-icon-256.png");
}
