<h1 align="center">
  <img src="./assets/brand/diskweave-logo.png" alt="DiskWeave" width="320">
</h1>

<p align="center">
  <strong>어떤 파일과 폴더가 용량을 차지하는지 한눈에 보는 Windows 디스크 분석 도구</strong>
</p>

<p align="center">
  <a href="https://github.com/ghminx/DiskWeave/releases/download/v0.1.1/DiskWeave-0.1.1-windows-x64-preview.zip"><strong>Windows x64 Portable ZIP 다운로드</strong></a>
  · <a href="https://github.com/ghminx/DiskWeave/releases/tag/v0.1.1">v0.1.1 프리뷰</a>
  · <a href="#시작하기">시작하기</a>
</p>

![DiskWeave에서 폴더별 용량, 확장자 통계, 공간 지도를 함께 보는 화면](docs/ui/diskweave-overview.png)

*예시 데이터로 표시한 DiskWeave 분석 화면*

## DiskWeave로 할 수 있는 일

| 작업 | 사용 예시 |
| --- | --- |
| **디스크 공간 분석** | 드라이브나 폴더를 스캔해 용량이 큰 폴더를 찾고, 공간 지도에서 용량 비중을 확인함. |
| **파일 검색과 정리** | 스캔한 위치, 지정 폴더 또는 로컬 고정 드라이브 전체에서 이름·경로로 검색함. 파일 목록에서 복사·이동·휴지통 이동을 수행함. |
| **중복 파일 찾기** | 선택한 범위에서 내용이 같은 파일을 묶어 보여 주고, 하드 링크는 복사본과 구분함. |
| **폴더 나란히 탐색** | 폴더를 새 탭이나 오른쪽 창에 열어 두 위치를 오가며 확인함. |
| **파일 변경 확인** | 화면에 열린 폴더의 파일 추가·수정·삭제를 자동으로 반영함. NTFS 실시간 갱신을 켜면 더 넓은 범위의 변경도 추적 가능함. |
| **분석 결과 기록·비교** | 파일 목록을 CSV로 내보내거나, 분석 스냅샷 두 개의 항목 및 용량 변화를 비교함. |

## 시작하기

1. Windows 10/11 x64에서 **Portable ZIP**을 다운로드한 뒤 원하는 폴더에 압축을 풂. 다운로드에는 저장소 읽기 권한이 필요함.
2. **DiskWeave.exe**를 실행한 뒤 분석할 드라이브나 폴더를 선택하고 **스캔 시작**을 누름.
3. 왼쪽 폴더 트리와 아래쪽 **공간 지도**에서 용량을 확인함. 아직 스캔하지 않은 위치의 파일을 찾으려면 상단 **빠른 검색**에서 **폴더 지정** 또는 **내 PC 전체**를 선택함.

일반 스캔은 관리자 권한 없이 동작함. 파일 변경은 사용자가 확인한 뒤 수행하며, NTFS 실시간 갱신을 시작할 때 Windows 관리자 승인이 필요함. [Portable 사용 안내](docs/portable-readme.md) · [릴리스 노트](docs/releases/v0.1.1.md)

## 프리뷰 참고 사항

- 개발 PC에서 C드라이브 전체 NTFS 실시간 갱신의 첫 목록 표시에 약 11분 소요됨. 기준 목록이 만들어진 뒤 시험 파일의 생성·수정·삭제는 약 1초에 반영됨. [실측 기록](docs/usn.md#최신-코드-검토-후-전체-c드라이브-gui-대조)
- 이 프리뷰의 실행 파일은 미서명 상태이며 일부 Windows 11 환경에서 Smart App Control에 의해 차단될 수 있음.

## 소스에서 실행

Windows x64에서 Rust stable, MSVC 빌드 도구 및 Windows SDK를 준비한 뒤 실행함.

~~~powershell
cargo run --release
cargo run --release -- "D:\Workspace" --find "Video"
pwsh -NoProfile -File tools/Build-Portable.ps1
~~~

[프로젝트 사양](PROJECT_SPEC.md) · [코드 구조](docs/architecture.md) · [개발·배포 검증](docs/usability-release.md)

## 라이선스

DiskWeave는 [Apache License 2.0](LICENSE)으로 제공함. [NOTICE](NOTICE)에 프로젝트 고지를 기록함. Portable ZIP에는 내장 Pretendard 글꼴과 의존성의 별도 라이선스 고지를 포함함.
