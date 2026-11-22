<h1 align="center">
  <img src="assets/brand/diskweave-logo.png" alt="DiskWeave" width="320">
</h1>

<p align="center"><strong>디스크 사용량을 한눈에 확인하는 Windows 파일 탐색 도구</strong></p>

<p align="center">
  폴더별 용량을 살펴보고, 필요한 파일을 찾아 같은 화면에서 정리할 수 있는 데스크톱 앱
</p>

<p align="center">
  <a href="https://github.com/ghminx/DiskWeave/releases/download/v0.1.1/DiskWeave-0.1.1-windows-x64-preview.zip"><strong>Windows x64 다운로드</strong></a>
  · <a href="#시작하기">시작하기</a>
  · <a href="docs/portable-readme.md">사용 안내</a>
  · <a href="https://github.com/ghminx/DiskWeave/releases/tag/v0.1.1">v0.1.1 프리뷰</a>
</p>

<p align="center"><sub>Windows 10/11 x64 · 설치 없이 실행하는 Portable ZIP</sub></p>

![DiskWeave에서 폴더 트리와 파일 목록, 확장자별 통계, 공간 지도를 함께 보는 화면](docs/ui/diskweave-overview.png)

*예시 데이터로 구성한 DiskWeave 분석 화면*

## 시작하기

1. **Windows x64 다운로드**에서 받은 ZIP을 원하는 폴더에 압축 해제. 다운로드에는 저장소 읽기 권한이 필요함.
2. `DiskWeave.exe`를 실행하고 드라이브나 폴더를 선택한 뒤 **스캔 시작**.
3. **폴더 탐색**과 **공간 지도**에서 용량이 큰 폴더를 확인. 현재 분석 범위 밖의 파일은 **빠른 검색**의 **폴더 지정** 또는 **내 PC 전체**에서 이름이나 경로로 검색 가능함.

## 이렇게 활용할 수 있음

| 상황 | DiskWeave에서 할 수 있는 일 |
| --- | --- |
| **드라이브 여유 공간이 줄었을 때** | 폴더별 크기를 공간 지도로 보고, 대용량 파일 목록과 확장자별 통계로 용량을 차지하는 항목을 찾음. |
| **여러 폴더의 파일을 정리할 때** | 두 폴더를 좌우 창이나 별도 탭에 열고 파일을 복사·이동하거나 휴지통으로 보냄. 열린 폴더의 파일 변경은 자동 반영됨. |
| **복사본이 쌓였을 때** | 파일 내용을 대조해 중복 그룹을 찾고, 같은 파일을 가리키는 하드 링크는 별도로 확인함. |
| **정리 전후를 비교할 때** | 파일·폴더 목록을 CSV로 내보내거나 두 시점에 저장한 분석 스냅샷을 비교해 추가·삭제 항목과 용량 변화를 확인함. |

일반 스캔은 관리자 권한 없이 가능하며, 파일 변경 작업은 사용자 확인 후 수행함. 로컬 NTFS 드라이브 또는 지정 폴더의 변경을 추적하는 **NTFS 실시간 갱신**은 별도로 켤 수 있으며 Windows 관리자 승인이 필요함.

## 프리뷰에서 확인한 범위

- ZIP을 별도 폴더에 풀어 개발 PC에서 GUI 및 CLI 실행을 확인함. 다른 Windows PC에서의 실행은 검증 예정임.
- 개발 PC에서 C드라이브 전체 NTFS 실시간 갱신의 **첫 목록 표시**에 약 11분 소요됨. 첫 목록이 표시된 뒤 시험 파일 생성·수정·삭제는 각각 약 1초에 반영됨. [실측 기록](docs/usn.md#최신-코드-검토-후-전체-c드라이브-gui-대조)
- 개발 PC에서는 Smart App Control을 켠 상태에서 실행을 확인함. 미서명 프리뷰이므로 다른 환경에서는 Windows의 평판 판정에 따라 실행이 차단될 수 있음. [v0.1.1 릴리스 노트](docs/releases/v0.1.1.md)

## 소스에서 실행

Windows x64에 Rust stable, MSVC 빌드 도구 및 Windows SDK를 준비한 뒤 실행함.

~~~powershell
# GUI
cargo run --release

# CLI에서 폴더 스캔과 파일 검색
cargo run --release -- "D:\Workspace" --find "Video"

# Portable ZIP 생성
pwsh -NoProfile -File tools/Build-Portable.ps1
~~~

[프로젝트 사양](PROJECT_SPEC.md) · [코드 구조](docs/architecture.md) · [빌드 및 배포 검증](docs/usability-release.md)

## License

DiskWeave is licensed under the [Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for project attribution. The portable ZIP includes license notices for bundled dependencies and the Pretendard font.
