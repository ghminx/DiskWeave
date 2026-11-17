param([string]$Executable, [switch]$WithoutTray)
$ErrorActionPreference='Stop'
Add-Type @"
using System;using System.Runtime.InteropServices;using System.Text;
public static class QuickNativeCheck {
 [StructLayout(LayoutKind.Sequential)]struct IconId {public uint cbSize;public IntPtr window;public uint id;public Guid guid;}
 [StructLayout(LayoutKind.Sequential)]struct Rect {public int Left,Top,Right,Bottom;}
 [DllImport("shell32.dll")] static extern int Shell_NotifyIconGetRect(ref IconId id,out Rect r);
 public static bool TrayReady(uint owner){var h=Find(owner,"DiskWeaveQuickSearchHost");if(h==IntPtr.Zero)return false;var id=new IconId{cbSize=(uint)Marshal.SizeOf(typeof(IconId)),window=h,id=1};Rect r;return Shell_NotifyIconGetRect(ref id,out r)==0;}
 [DllImport("user32.dll",SetLastError=true)]static extern bool RegisterHotKey(IntPtr h,int id,uint modifiers,uint key);
 [DllImport("user32.dll",SetLastError=true)]static extern bool UnregisterHotKey(IntPtr h,int id);
 public static bool KeyAvailable(){
  if(RegisterHotKey(IntPtr.Zero,0x41C7,0x4007,135)){
   if(!UnregisterHotKey(IntPtr.Zero,0x41C7))throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   return true;
  }
  int error=Marshal.GetLastWin32Error();
  if(error!=1409)throw new System.ComponentModel.Win32Exception(error);
  return false;
 }
 delegate bool Visit(IntPtr h,IntPtr p);
 [DllImport("user32.dll")] static extern bool EnumWindows(Visit v,IntPtr p);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetWindowTextW(IntPtr h,StringBuilder b,int n);
 [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 public static IntPtr Find(uint owner,string name){IntPtr result=IntPtr.Zero;EnumWindows((h,p)=>{uint id;GetWindowThreadProcessId(h,out id);if(id==owner){var text=new StringBuilder(256);GetWindowTextW(h,text,256);if(text.ToString()==name){result=h;return false;}}return true;},IntPtr.Zero);return result;}
}
"@
function Until([scriptblock]$Check,[string]$Description){
 $deadline=[DateTime]::UtcNow.AddSeconds(8)
 do {if($script:app.HasExited){throw "앱 종료: $Description"};if(& $Check){return};[Threading.Thread]::Sleep(20)}while([DateTime]::UtcNow-lt $deadline)
 throw "시간 초과: $Description"
}
$repo=Split-Path $PSScriptRoot -Parent
if(-not $Executable){$Executable=Join-Path $repo 'target/release/diskweave.exe'}
$trial=Join-Path $repo ('target/quick-native-check-'+[Guid]::NewGuid().ToString('N'))
$settings=Join-Path $trial 'DiskWeave'
New-Item -ItemType Directory -Path $settings -Force|Out-Null
@{version=1;enabled=$true;stay=(-not $WithoutTray);shortcut=@{modifiers=7;key=135}}|ConvertTo-Json -Compress|Set-Content -LiteralPath (Join-Path $settings 'preferences.json')
$previousLocal=[Environment]::GetEnvironmentVariable('LOCALAPPDATA','Process')
$script:app=$null
try {
 $env:LOCALAPPDATA=$trial
 $stderr=Join-Path $trial 'stderr.log'
 if(-not [QuickNativeCheck]::KeyAvailable()){throw '시험용 Ctrl+Alt+Shift+F24가 이미 사용 중임'}
 $script:app=Start-Process -FilePath $Executable -WindowStyle Hidden -PassThru -RedirectStandardError $stderr
 $owner=[uint32]$script:app.Id
 Until { [QuickNativeCheck]::Find($owner,'DiskWeaveQuickSearchHost') -ne [IntPtr]::Zero } '네이티브 호스트 준비'
 $hostWindow=[QuickNativeCheck]::Find($owner,'DiskWeaveQuickSearchHost')
 Until {-not [QuickNativeCheck]::KeyAvailable()} '시험 단축키 등록 완료'
 Until { $root=[QuickNativeCheck]::Find($owner,'DiskWeave');$root-ne[IntPtr]::Zero -and [QuickNativeCheck]::IsWindowVisible($root) } '메인 창 표시'
 $root=[QuickNativeCheck]::Find($owner,'DiskWeave')
 if(-not $WithoutTray){Until { [QuickNativeCheck]::TrayReady($owner) } '트레이 등록 완료'}
 if($WithoutTray){
  [QuickNativeCheck]::PostMessageW($root,0x112,[IntPtr]::new(0xF020),[IntPtr]::Zero)|Out-Null
  Until {[QuickNativeCheck]::IsIconic($root)} '메인 창 최소화'
 }else{
  if(-not [QuickNativeCheck]::PostMessageW($root,0x10,[IntPtr]::Zero,[IntPtr]::Zero)){throw '종료 요청 전달 실패'}
  Until {-not [QuickNativeCheck]::IsWindowVisible($root)} '트레이 숨김'
 }
 for($round=1;$round -le 3;$round++){
  if(-not [QuickNativeCheck]::PostMessageW($hostWindow,0x312,[IntPtr]::new(1),[IntPtr]::Zero)){throw '단축키 이벤트 전달 실패'}
  Until { $q=[QuickNativeCheck]::Find($owner,'DiskWeave · 빠른 검색');$q-ne[IntPtr]::Zero -and [QuickNativeCheck]::IsWindowVisible($q)} '메인 비활성 상태의 검색창 표시'
  $q=[QuickNativeCheck]::Find($owner,'DiskWeave · 빠른 검색')
  if(-not [QuickNativeCheck]::PostMessageW($q,0x10,[IntPtr]::Zero,[IntPtr]::Zero)){throw '검색창 닫기 실패'}
  Until {-not [QuickNativeCheck]::IsWindowVisible($q)} '검색창만 숨김'
 }
 if($WithoutTray){
  [QuickNativeCheck]::PostMessageW($root,0x10,[IntPtr]::Zero,[IntPtr]::Zero)|Out-Null
  if(-not $script:app.WaitForExit(8000)){throw '상주 OFF의 정상 종료 실패'}
  'PASS: minimized main, 3 quick-window cycles, normal exit with tray disabled'
 }else{
  if(-not [QuickNativeCheck]::PostMessageW($hostWindow,0x8020,[IntPtr]::new(1),[IntPtr]::new(0x203))){throw '트레이 복원 알림 실패'}
  Until {[QuickNativeCheck]::IsWindowVisible($root)} '트레이 알림으로 메인 창 복원'
  [QuickNativeCheck]::PostMessageW($root,0x112,[IntPtr]::new(0xF020),[IntPtr]::Zero)|Out-Null
  Until {[QuickNativeCheck]::IsIconic($root)} '복원 전 최소화'
  [QuickNativeCheck]::PostMessageW($hostWindow,0x8020,[IntPtr]::new(1),[IntPtr]::new(0x203))|Out-Null
  Until {[QuickNativeCheck]::IsWindowVisible($root) -and -not [QuickNativeCheck]::IsIconic($root)} '최소화한 메인 창 복원'
  'PASS: close-to-tray, 3 quick cycles, hidden/minimized main restore' 
 }
} catch {
 if($script:app -and $script:app.HasExited){$script:app.WaitForExit()}
 if(Test-Path $stderr){Get-Content -LiteralPath $stderr -Tail 10}
 throw
} finally {
 [Environment]::SetEnvironmentVariable('LOCALAPPDATA',$previousLocal,'Process')
 if($script:app -and -not $script:app.HasExited){Stop-Process -Id $script:app.Id}
}
