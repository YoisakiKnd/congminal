// 启用并切换到指定的输入源（输入法），用于 CI 上的输入法测试。
// 用法：swift select_input_source.swift <输入源 ID>
import Carbon

let wanted = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "com.apple.inputmethod.SCIM.ITABC"

func sourceID(_ source: TISInputSource) -> String {
    guard let property = TISGetInputSourceProperty(source, kTISPropertyInputSourceID) else { return "?" }
    return Unmanaged<CFString>.fromOpaque(property).takeUnretainedValue() as String
}

let all = TISCreateInputSourceList(nil, true).takeRetainedValue() as! [TISInputSource]
print("中文相关的输入源：", all.map(sourceID).filter { $0.contains("SCIM") || $0.contains("Chinese") })
guard let source = all.first(where: { sourceID($0) == wanted }) else {
    print("找不到输入源 \(wanted)")
    exit(1)
}
print("启用：", TISEnableInputSource(source))
print("选中：", TISSelectInputSource(source))
print("当前输入源：", sourceID(TISCopyCurrentKeyboardInputSource().takeRetainedValue()))
