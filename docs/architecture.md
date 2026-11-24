# 코드 구조와 테스트 안내

## 주요 진입점

| 작업 | 확인할 코드 |
| --- | --- |
| GUI·CLI 실행 | [`main.rs`](../src/main.rs), [`gui.rs`](../src/gui.rs), [`output.rs`](../src/output.rs) |
| 스캔·용량 분석 | [`scan_job.rs`](../src/scan_job.rs), [`scanner.rs`](../src/scanner.rs), [`analysis.rs`](../src/analysis.rs), [`treemap.rs`](../src/treemap.rs) |
| 탐색·검색 | [`workspace.rs`](../src/workspace.rs), [`search.rs`](../src/search.rs), [`quick_search.rs`](../src/quick_search.rs) |
| 열린 폴더 갱신·파일 관리 | [`visible_watch.rs`](../src/visible_watch.rs), [`folder_updates.rs`](../src/gui/folder_updates.rs), [`file_job.rs`](../src/file_job.rs), [`file_ops.rs`](../src/file_ops.rs) |
| 중복 검사·결과 보관 | [`duplicates.rs`](../src/duplicates.rs), [`export.rs`](../src/export.rs), [`snapshot.rs`](../src/snapshot.rs), [`comparison.rs`](../src/comparison.rs) |

스캔 작업은 읽은 메타데이터를 바탕으로 폴더별 통계를 계산함. 완료 전 미리보기와 직전 완료 결과는 별도로 처리함. 다른 폴더와 PC 전체 검색은 지정 위치를 직접 탐색하고, 스캔한 위치의 검색은 완료된 결과를 사용함. 열린 폴더의 자동 갱신은 해당 목록에 적용되며 전체 통계의 갱신에는 재스캔이 필요함.

분석 결과는 사용자가 CSV로 내보내거나 수동 스냅샷으로 저장할 수 있음. 앱 재시작 시 화면 설정은 불러오며 분석 목록은 새로 시작함.

## Windows 검증

Windows x64에서 Rust, MSVC 빌드 도구, Windows SDK, PowerShell 7 필요. 테스트에는 별도 임시 폴더 사용.

```powershell
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
pwsh -NoProfile -File tools/Build-Portable.ps1
pwsh -NoProfile -File tools/Test-PortableZip.ps1 -Zip <ZIP>
```

배포 빌드는 `target/dist/`에 ZIP을 생성함. [사용 안내](portable-readme.md)와 프로젝트·의존성 라이선스가 ZIP에 포함되며, 검증 스크립트는 압축을 풀어 CLI와 ZIP 구성을 확인함.
