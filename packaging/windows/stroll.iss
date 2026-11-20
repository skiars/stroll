[Setup]
AppId=io.stroll.Stroll
AppName=Stroll
AppVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\Stroll
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
SetupIconFile=stroll.ico
UninstallDisplayIcon={app}\stroll.exe
ChangesAssociations=yes
WizardStyle=modern

[Files]
Source: "{#BinDir}\stroll.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#VCRuntime}"; DestDir: "{app}"

[Icons]
Name: "{userprograms}\Stroll"; Filename: "{app}\stroll.exe"; WorkingDir: "{app}"

[Tasks]
Name: "contextmenu"; Description: "Add Open with Stroll to the folder context menu"
Name: "markdown"; Description: "Register Stroll in Open with for Markdown (.md) files"

#define Classes "Software\Classes"
#define FolderVerb Classes + "\Directory\shell\io.stroll.Stroll"
#define MarkdownProgId "io.stroll.Stroll.Markdown"
#define MarkdownHandler Classes + "\" + MarkdownProgId
#define MarkdownOpenWith Classes + "\.md\OpenWithProgids"

[Registry]
Root: HKCU; Subkey: "{#FolderVerb}"; ValueType: string; ValueData: "Open with Stroll"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCU; Subkey: "{#FolderVerb}"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\stroll.exe,0"; Tasks: contextmenu
Root: HKCU; Subkey: "{#FolderVerb}\command"; ValueType: string; ValueData: """{app}\stroll.exe"" ""%1"""; Tasks: contextmenu
Root: HKCU; Subkey: "{#FolderVerb}"; Flags: deletekey dontcreatekey; Tasks: not contextmenu

Root: HKCU; Subkey: "{#MarkdownHandler}"; ValueType: string; ValueData: "Markdown document"; Flags: uninsdeletekey; Tasks: markdown
Root: HKCU; Subkey: "{#MarkdownHandler}\DefaultIcon"; ValueType: string; ValueData: "{app}\stroll.exe,0"; Tasks: markdown
Root: HKCU; Subkey: "{#MarkdownHandler}\shell\open\command"; ValueType: string; ValueData: """{app}\stroll.exe"" ""%1"""; Tasks: markdown
Root: HKCU; Subkey: "{#MarkdownOpenWith}"; ValueType: string; ValueName: "{#MarkdownProgId}"; ValueData: ""; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: markdown
Root: HKCU; Subkey: "{#MarkdownHandler}"; Flags: deletekey dontcreatekey; Tasks: not markdown
Root: HKCU; Subkey: "{#MarkdownOpenWith}"; ValueName: "{#MarkdownProgId}"; Flags: deletevalue dontcreatekey; Tasks: not markdown
