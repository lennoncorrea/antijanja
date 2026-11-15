; Instalador do AntiJanja (por usuário, sem pedir administrador).
; Compatível com o instalador antigo (Tauri): mesma pasta, mesmo .exe e mesma
; entrada em "Adicionar ou remover programas", então atualiza no lugar.
;
; Parâmetros usados pela atualização automática:
;   /P ou /S  instala sem perguntar      /R  reabre o app no final
;   /UPDATE   é uma atualização (não recria o atalho da área de trabalho)
;
; makensis /DVERSION=3.0.0 /DEXE=caminho\antijanja.exe /DOUTFILE=saida.exe antijanja.nsi

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
  !error "Passe /DVERSION=x.y.z"
!endif

!define PRODUCT "AntiJanja"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT}"

Name "${PRODUCT}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\${PRODUCT}"
RequestExecutionLevel user
SetCompressor /SOLID lzma
BrandingText "${PRODUCT} ${VERSION}"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${PRODUCT}"
VIAddVersionKey "FileDescription" "Instalador do ${PRODUCT}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "${PRODUCT}"

!define MUI_ICON "..\assets\icons\icon.ico"
!define MUI_UNICON "..\assets\icons\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\antijanja.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Abrir o ${PRODUCT}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "PortugueseBR"

Var Params
Var IsUpdate
Var Restart

Function .onInit
  ${GetParameters} $Params
  ClearErrors
  ${GetOptions} $Params "/P" $0
  IfErrors +2 0
    SetSilent silent
  StrCpy $IsUpdate "0"
  ClearErrors
  ${GetOptions} $Params "/UPDATE" $0
  IfErrors +2 0
    StrCpy $IsUpdate "1"
  StrCpy $Restart "0"
  ClearErrors
  ${GetOptions} $Params "/R" $0
  IfErrors +2 0
    StrCpy $Restart "1"
FunctionEnd

Section "Instalar"
  SetOutPath "$INSTDIR"
  ; Fecha o app se ainda estiver aberto (a atualização o encerra, mas por garantia).
  nsExec::Exec 'taskkill /F /IM antijanja.exe'
  Sleep 400

  File "/oname=antijanja.exe" "${EXE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$\"$INSTDIR\antijanja.exe$\""
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PRODUCT}"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$\"$INSTDIR$\""
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" "$\"$INSTDIR\uninstall.exe$\" /S"
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" $0

  CreateShortCut "$SMPROGRAMS\${PRODUCT}.lnk" "$INSTDIR\antijanja.exe"
  StrCmp $IsUpdate "1" +2 0
    CreateShortCut "$DESKTOP\${PRODUCT}.lnk" "$INSTDIR\antijanja.exe"

  StrCmp $Restart "1" 0 +2
    Exec '"$INSTDIR\antijanja.exe"'
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /F /IM antijanja.exe'
  Sleep 400
  Delete "$INSTDIR\antijanja.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${PRODUCT}.lnk"
  Delete "$DESKTOP\${PRODUCT}.lnk"
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
