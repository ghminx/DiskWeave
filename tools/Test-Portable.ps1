param([Parameter(Mandatory)][string]$Executable, [switch]$Gui)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $Executable).Path
$bytes = [IO.File]::ReadAllBytes($exe)
$pe = [BitConverter]::ToInt32($bytes, 60)
if ([BitConverter]::ToUInt16($bytes, $pe + 4) -ne 0x8664) { throw 'x64 실행 파일이 아님' }
if ([BitConverter]::ToUInt16($bytes, $pe + 24 + 68) -ne 2) { throw 'GUI 기본 실행용 Windows subsystem이 아님' }
$version = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe)
$manifest = Get-Content -Raw -LiteralPath (Join-Path (Split-Path $PSScriptRoot) 'Cargo.toml')
if ($manifest -notmatch '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"') { throw 'Cargo.toml 제품 버전 누락' }
$expectedVersion = $Matches[1] + '.0'
if ($version.ProductName -ne 'DiskWeave' -or $version.FileVersion -ne $expectedVersion) { throw "제품·버전 리소스 불일치: $expectedVersion" }
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class PortableProbe {
 [DllImport("kernel32", CharSet=CharSet.Unicode)] public static extern IntPtr LoadLibraryEx(string p,IntPtr f,uint flags);
 [DllImport("kernel32")] public static extern IntPtr FindResource(IntPtr h,IntPtr name,IntPtr type);
 [DllImport("kernel32")] public static extern bool FreeLibrary(IntPtr h);
 public delegate bool EnumProc(IntPtr h,IntPtr p);
 [DllImport("user32")] public static extern bool EnumWindows(EnumProc f,IntPtr p);
 [DllImport("user32")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32",CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h,System.Text.StringBuilder s,int n);
 [DllImport("user32")] public static extern bool IsZoomed(IntPtr h);
 [DllImport("user32")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
 [DllImport("user32")] public static extern uint GetDpiForWindow(IntPtr h);
 [StructLayout(LayoutKind.Sequential)] public struct Rect {public int Left,Top,Right,Bottom;}
 [StructLayout(LayoutKind.Sequential)] public struct Monitor {public uint Size;public Rect Bounds,Work;public uint Flags;}
 [DllImport("user32")] public static extern bool GetWindowRect(IntPtr h,out Rect r);
 [DllImport("user32")] public static extern IntPtr MonitorFromWindow(IntPtr h,uint flags);
 [DllImport("user32",CharSet=CharSet.Unicode)] public static extern bool GetMonitorInfo(IntPtr h,ref Monitor m);
 [DllImport("user32")] public static extern bool PostMessage(IntPtr h,uint msg,IntPtr w,IntPtr l);
 public static bool CloseWindow(int process) {
  var h=Window(process);return h!=IntPtr.Zero && PostMessage(h,0x10,IntPtr.Zero,IntPtr.Zero);
 }
 public static IntPtr Window(int process) {
  IntPtr found=IntPtr.Zero;
  EnumWindows((h,p)=>{uint pid;GetWindowThreadProcessId(h,out pid);if(pid==process && IsWindowVisible(h)){var s=new System.Text.StringBuilder(256);GetWindowText(h,s,256);if(s.ToString()=="DiskWeave")found=h;}return true;},IntPtr.Zero);
  return found;
 }
 public static bool RestoredBounds(IntPtr h) {
  IntPtr previous=SetThreadDpiAwarenessContext(new IntPtr(-4));
  try {
   Rect r;var m=new Monitor();m.Size=(uint)Marshal.SizeOf(typeof(Monitor));
   if(!GetWindowRect(h,out r)||!GetMonitorInfo(MonitorFromWindow(h,2),ref m))return false;
   double scale=GetDpiForWindow(h)/96.0;
   BoundsInfo=String.Format("window={0},{1},{2},{3}; work={4},{5},{6},{7}; scale={8}",r.Left,r.Top,r.Right,r.Bottom,m.Work.Left,m.Work.Top,m.Work.Right,m.Work.Bottom,scale);
   return scale>0 && Math.Abs((r.Right-r.Left)/scale-960)<4 && Math.Abs((r.Bottom-r.Top)/scale-640)<4 && r.Left>=m.Work.Left && r.Top>=m.Work.Top && r.Right<=m.Work.Right && r.Bottom<=m.Work.Bottom;
  } finally {SetThreadDpiAwarenessContext(previous);}
 }
 public static string BoundsInfo="";
}
'@
$library = [PortableProbe]::LoadLibraryEx($exe, [IntPtr]::Zero, 2)
if ($library -eq [IntPtr]::Zero) { throw 'PE 리소스 열기 실패' }
try {
    foreach ($type in @(14,24)) {
        if ([PortableProbe]::FindResource($library, [IntPtr]1, [IntPtr]$type) -eq [IntPtr]::Zero) { throw "필수 리소스 누락: $type" }
    }
} finally { [void][PortableProbe]::FreeLibrary($library) }
$imports = & dumpbin.exe /nologo /dependents $exe 2>&1
if ($LASTEXITCODE -ne 0) { throw 'DLL 검사 실패' }
$dlls = @($imports | ForEach-Object { if ($_ -match '^\s+([\w.-]+\.dll)\s*$') { $Matches[1] } })
if ($dlls.Count -eq 0) { throw 'DLL 검사 결과 없음' }
foreach ($dll in $dlls) {
    if ($dll -match '^(VCRUNTIME|MSVCP)' -or (!(Test-Path -LiteralPath (Join-Path $env:WINDIR "System32/$dll")) -and $dll -notmatch '^api-ms-win-')) { throw "별도 런타임 필요: $dll" }
}
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('DiskWeave 배포 시험 '+[guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory((Join-Path $fixture '하위 폴더'))
[IO.File]::WriteAllBytes((Join-Path $fixture '하위 폴더/자료.bin'), [byte[]](1,2,3))
[IO.File]::WriteAllBytes((Join-Path $fixture '직계.bin'), [byte[]](4,5,6,7,8))
function Invoke-Cli([string[]]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new($exe)
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.Encoding]::UTF8
    $info.StandardErrorEncoding = [Text.Encoding]::UTF8
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $p = [Diagnostics.Process]::Start($info)
    try {
        $stdout = $p.StandardOutput.ReadToEndAsync(); $stderr = $p.StandardError.ReadToEndAsync()
        if (!$p.WaitForExit(30000)) { $p.Kill(); throw 'CLI 종료 시간 초과' }
        return @{Code=$p.ExitCode; Out=$stdout.GetAwaiter().GetResult(); Err=$stderr.GetAwaiter().GetResult()}
    } finally { $p.Dispose() }
}
try {
    $tree = Invoke-Cli @($fixture)
    if ($tree.Code -ne 0 -or !$tree.Out.Contains("8`t2`t1`t$fixture") -or !$tree.Out.Contains('하위 폴더')) { throw ('CLI 집계·한글 경로 실패: '+$tree.Err) }
    $find = Invoke-Cli @($fixture,'--find','하위')
    if ($find.Code -ne 0 -or !$find.Out.Contains('하위 폴더')) { throw 'CLI 검색 실패' }
    $at = Invoke-Cli @($fixture,'--at','하위 폴더')
    if ($at.Code -ne 0 -or !$at.Out.Contains("3`t1`t0`t")) { throw 'CLI 하위 경로 실패' }
    $bad = Invoke-Cli @($fixture,'--invalid')
    if ($bad.Code -ne 1 -or !$bad.Err.Contains('사용법')) { throw 'CLI 오류·종료 코드 실패' }
    if ($Gui) {
      $profile = Join-Path $fixture '설정'
      $preferences = Join-Path $profile 'DiskWeave/preferences.json'
      foreach ($mode in @('default','restored','maximized')) {
        if ($mode -ne 'default') {
            $settings = Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json
            $settings.ui.window = @{position=@(-90000,-90000);size=@(960,640);maximized=($mode -eq 'maximized')}
            $settings.ui.map_height = 180
            $settings.ui.workers = 2
            [IO.File]::WriteAllText($preferences,($settings | ConvertTo-Json -Depth 8))
        }
        $info = [Diagnostics.ProcessStartInfo]::new($exe)
        $info.UseShellExecute = $false
        $info.Environment['LOCALAPPDATA'] = $profile
        $p = [Diagnostics.Process]::Start($info)
        try {
            $limit = [DateTime]::UtcNow.AddSeconds(20); $closed = $false
            while ([DateTime]::UtcNow -lt $limit -and !$p.HasExited) {
                $window = [PortableProbe]::Window($p.Id)
                if ($window -ne [IntPtr]::Zero) {
                    Start-Sleep -Milliseconds 1200
                    if ($mode -eq 'restored' -and ![PortableProbe]::RestoredBounds($window)) { throw ('저장된 창 크기·화면 경계 복원 실패: '+[PortableProbe]::BoundsInfo) }
                    if ($mode -eq 'maximized' -and ![PortableProbe]::IsZoomed($window)) { throw '최대화 복원 실패' }
                    $closed=[PortableProbe]::CloseWindow($p.Id); break
                }
                Start-Sleep -Milliseconds 200
            }
            if (!$closed -or !$p.WaitForExit(15000) -or $p.ExitCode -ne 0) { throw '독립 폴더 GUI 실행·종료 실패' }
        } finally { if (!$p.HasExited) { $p.Kill() }; $p.Dispose() }
        if (!(Test-Path -LiteralPath $preferences)) { throw '종료 시 화면 설정 저장 실패' }
        $json = [IO.File]::ReadAllText($preferences)
        if ($json.Contains($fixture) -or $json.Contains('하위 폴더') -or $json.Contains('sample')) { throw '설정에 분석 경로·검색어 저장' }
        if ($mode -ne 'default') {
            $saved = $json | ConvertFrom-Json
            if ($saved.ui.workers -ne 2 -or $saved.ui.map_height -ne 180) { throw '재시작 후 화면 설정 손실' }
        }
      }
    }
} finally {
    # 직접 생성한 개별 파일만 제거하고 설정 시험 폴더는 후속 진단용 보존
    [IO.File]::Delete((Join-Path $fixture '하위 폴더/자료.bin'))
    [IO.File]::Delete((Join-Path $fixture '직계.bin'))
    [IO.Directory]::Delete((Join-Path $fixture '하위 폴더'))
}
Write-Output ('PASS: x64, GUI subsystem, version/icon/manifest, system DLLs, CLI 4 cases'+$(if($Gui){', GUI default/restart/bounds/maximized/settings'}))
