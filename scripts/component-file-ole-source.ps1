[CmdletBinding()]
param([string]$Plan,[string]$CompileReport,[switch]$CompileOnly,[switch]$OleInputApproved,[switch]$OleCloseWhileDragging,[switch]$OleManualSourceFocusApproved)
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
if([Threading.Thread]::CurrentThread.ApartmentState -ne 'STA'){throw 'Explicit Windows PowerShell -STA required'}
Add-Type -AssemblyName System.Windows.Forms,System.Drawing,System.Web.Extensions
Add-Type -ReferencedAssemblies System.Windows.Forms,System.Drawing,System.Web.Extensions -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Web.Script.Serialization;
using System.Windows.Forms;

public sealed class OwnedOleFileSource : Form {
  const long Marker=0x43464534;
  const string Property="GPUI_CEF_OLE_Source_R58";
  [StructLayout(LayoutKind.Sequential)] struct InputMessageSource {public uint deviceType;public uint originId;}
  [DllImport("user32.dll")] static extern bool GetCurrentInputMessageSource(out InputMessageSource source);
  [DllImport("user32.dll")] static extern short GetAsyncKeyState(int key);
  [DllImport("user32.dll")] static extern short GetKeyState(int key);
  [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);
  [DllImport("user32.dll")] static extern IntPtr GetMessageExtraInfo();
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool SetProp(IntPtr h,string n,IntPtr v);
  [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern IntPtr GetProp(IntPtr h,string n);
  [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern IntPtr RemoveProp(IntPtr h,string n);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
  [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
  readonly string directory;
  readonly string[] paths;
  readonly IntPtr nonce;
  readonly bool closeWhileDragging, manualSourceFocus;
  readonly System.Windows.Forms.Timer timer;
  Point pressed;
  bool down,used,inDrag,abort,downMarked,downReceiptSaved,cancelOnRelease,terminalCancelOnRelease;
  long moves,queries;
  int terminalKeyState;
  string terminalAction;
  IntPtr ownedHwnd;
  readonly Stopwatch life=Stopwatch.StartNew();
  public static int InputSourceAbiBytes() {return Marshal.SizeOf(typeof(InputMessageSource));}
  public static void Save(string path,object value) {
    byte[] b=new UTF8Encoding(false).GetBytes(new JavaScriptSerializer().Serialize(value));
    using(var f=new FileStream(path,FileMode.CreateNew,FileAccess.Write,FileShare.Read)){f.Write(b,0,b.Length);}
  }
  static string Hash(string path) {using(var s=File.OpenRead(path))using(var h=SHA256.Create()){return BitConverter.ToString(h.ComputeHash(s)).Replace("-","").ToLowerInvariant();}}
  Dictionary<string,object> Identity() {
    uint p;uint thread=GetWindowThreadProcessId(Handle,out p);
    if(!IsWindow(Handle)||p!=Process.GetCurrentProcess().Id||thread!=GetCurrentThreadId()||GetProp(Handle,Property)!=nonce)
      throw new InvalidOperationException("owned source identity lost");
    var rect=RectangleToScreen(ClientRectangle);
    return new Dictionary<string,object>{{"hwnd",Handle.ToInt64()},{"pid",p},{"thread",thread},{"property",Property},{"nonce",nonce.ToInt64()},
      {"creation_ticks",Process.GetCurrentProcess().StartTime.ToUniversalTime().Ticks},{"dpi",GetDpiForWindow(Handle)},
      {"client_left",rect.Left},{"client_top",rect.Top},{"client_width",rect.Width},{"client_height",rect.Height},
      {"visible",IsWindowVisible(Handle)},{"iconic",IsIconic(Handle)},{"foreground",GetForegroundWindow()==Handle}};
  }
  public OwnedOleFileSource(string dir,string[] files,int value,bool closeCase,bool manualFocus) {
    directory=Path.GetFullPath(dir);paths=(string[])files.Clone();nonce=new IntPtr(value);closeWhileDragging=closeCase;manualSourceFocus=manualFocus;
    if(paths.Length!=2||value<=0)throw new ArgumentException("two owned files and positive nonce required");
    foreach(var p in paths)if(!Path.IsPathRooted(p)||!File.Exists(p))throw new ArgumentException("owned source file absent");
    Text=manualSourceFocus?"OLE SOURCE - select this window":"GPUI-CEF owned OLE source";
    StartPosition=FormStartPosition.Manual;Location=new Point(740,80);
    ClientSize=new Size(280,150);BackColor=manualSourceFocus?Color.FromArgb(0,91,80):Color.FromArgb(48,68,88);
    timer=new System.Windows.Forms.Timer();timer.Interval=100;
    timer.Tick+=delegate {CheckStop();};timer.Start();
  }
  void CheckStop() {
    bool requested=false;
    try {
      string file=Path.Combine(directory,"ole-source-stop.request");
      int pid=Process.GetCurrentProcess().Id;
      string identity=pid+":"+nonce.ToInt64();
      requested=File.Exists(file)&&File.ReadAllText(file)=="ole-stop:"+identity;
      if(!requested&&!abort&&closeWhileDragging&&inDrag&&!cancelOnRelease) {
        string arm=Path.Combine(directory,"ole-source-cancel-on-release.request");
        if(File.Exists(arm)&&File.ReadAllText(arm)=="ole-arm-cancel:"+identity) {
          cancelOnRelease=true;
          Save(Path.Combine(directory,"ole-source-cancel-armed.json"),new Dictionary<string,object>{{"pid",pid},{"nonce",nonce.ToInt64()},{"in_drag",true}});
        }
      }
    } catch(Exception x) {abort=true;Console.Error.WriteLine("owned source control read: "+x.Message);}
    if(!requested&&life.ElapsedMilliseconds<=45000&&!abort)return;
    abort=true;
    if(inDrag)return; // actual OLE callback cancels; never synthesize a foreign UP.
    timer.Stop();
    try {Save(Path.Combine(directory,"ole-source-stop-ack.json"),new Dictionary<string,object>{{"pid",Process.GetCurrentProcess().Id},{"nonce",nonce.ToInt64()},{"driver_stop",requested},{"in_drag",false}});}
    catch(Exception x){Console.Error.WriteLine("owned source stop record: "+x.Message);}
    Close();
  }
  protected override bool ShowWithoutActivation {get{return true;}}
  protected override void OnShown(EventArgs e) {
    base.OnShown(e);
    ownedHwnd=Handle;
    if(!SetProp(ownedHwnd,Property,nonce))throw new InvalidOperationException("source property install failed");
    var facts=Identity();var files=new List<object>();
    foreach(var p in paths)files.Add(new Dictionary<string,object>{{"name",Path.GetFileName(p)},{"bytes",new FileInfo(p).Length},{"sha256",Hash(p)}});
    facts.Add("files",files);facts.Add("runtime",Environment.Version.ToString());facts.Add("winforms",typeof(Form).Assembly.GetName().Version.ToString());
    facts.Add("manual_source_focus",manualSourceFocus);
    Save(Path.Combine(directory,"ole-source-ready.json"),facts);
  }
  protected override void OnPaint(PaintEventArgs e) {
    base.OnPaint(e);
    string text=manualSourceFocus?"OLE SOURCE\nClick and release THIS title bar.\nThen leave the mouse alone.":"Owned files only\nOne marked LEFT drag\nCOPY; no clipboard / foreground API";
    using(var brush=new SolidBrush(Color.White))e.Graphics.DrawString(text,Font,brush,new PointF(12,20));
  }
  protected override void WndProc(ref Message m) {
    if(m.Msg==0x0201||m.Msg==0x0204) {
      long extra=GetMessageExtraInfo().ToInt64();
      InputMessageSource source;bool sourceAvailable=GetCurrentInputMessageSource(out source);
      bool marked=(extra & ~1L)==Marker;
      if(m.Msg==0x0201)downMarked=marked;
      if(!downReceiptSaved) {
        downReceiptSaved=true;
        try {Save(Path.Combine(directory,"ole-source-first-button.json"),new Dictionary<string,object>{{"pid",Process.GetCurrentProcess().Id},{"nonce",nonce.ToInt64()},{"hwnd",Handle.ToInt64()},{"thread",GetCurrentThreadId()},{"message",m.Msg},{"extra_info",marked?(object)extra:null},{"extra_info_zero",extra==0},{"extra_info_high32_set",(unchecked((ulong)extra)>>32)!=0},{"marked",marked},{"input_source_available",sourceAvailable},{"input_device_type",source.deviceType},{"input_origin_id",source.originId},{"source_async_left",(int)(ushort)GetAsyncKeyState(1)},{"source_async_right",(int)(ushort)GetAsyncKeyState(2)},{"source_key_left",(int)(ushort)GetKeyState(1)},{"source_key_right",(int)(ushort)GetKeyState(2)},{"swap_buttons",GetSystemMetrics(23)},{"wparam",m.WParam.ToInt64()},{"client_x",(short)(m.LParam.ToInt64() & 65535)},{"client_y",(short)((m.LParam.ToInt64()>>16) & 65535)}});}
        catch(Exception x){abort=true;Console.Error.WriteLine("owned source button record: "+x.Message);}
      }
    }
    base.WndProc(ref m);
  }
  protected override void OnMouseDown(MouseEventArgs e) {
    base.OnMouseDown(e);if(e.Button!=MouseButtons.Left||used||!downMarked)return;
    Identity();if(GetForegroundWindow()!=Handle||!IsWindowVisible(Handle)||IsIconic(Handle))return;
    down=true;pressed=e.Location;
  }
  protected override void OnMouseMove(MouseEventArgs e) {
    base.OnMouseMove(e);moves++;
    if(!down||used||e.Button!=MouseButtons.Left)return;
    if(Math.Abs(e.X-pressed.X)<SystemInformation.DragSize.Width&&Math.Abs(e.Y-pressed.Y)<SystemInformation.DragSize.Height)return;
    Identity();if(GetForegroundWindow()!=Handle)return;
    used=true;down=false;inDrag=true;
    Save(Path.Combine(directory,"ole-source-started.json"),new Dictionary<string,object>{{"identity",Identity()},{"marked_left_down",downMarked},{"offered_format","FileDrop"},{"files",paths.Length},{"allowed_effect","Copy"}});
    DragDropEffects effect=DragDropEffects.None;string error=null;
    try {
      var data=new DataObject();data.SetData(DataFormats.FileDrop,false,paths);
      effect=DoDragDrop(data,DragDropEffects.Copy);
    } catch(Exception x){error=x.GetType().FullName+": "+x.Message;}
    finally {
      inDrag=false;timer.Stop();
      Save(Path.Combine(directory,"ole-source-result.json"),new Dictionary<string,object>{{"pid",Process.GetCurrentProcess().Id},{"nonce",nonce.ToInt64()},{"effect",(int)effect},
        {"error",error},{"aborted",abort},{"queries",queries},{"moves",moves},{"physical_source",queries>0},{"marked_left_down",downMarked},{"terminal_action",terminalAction},{"terminal_key_state",terminalKeyState},{"terminal_cancel_on_release",terminalCancelOnRelease}});
      BeginInvoke(new Action(Close));
    }
  }
  protected override void OnQueryContinueDrag(QueryContinueDragEventArgs e) {
    queries++;
    if(queries==1)Save(Path.Combine(directory,"ole-source-native-ready.json"),new Dictionary<string,object>{{"pid",Process.GetCurrentProcess().Id},{"nonce",nonce.ToInt64()},{"query",queries},{"key_state",e.KeyState},{"escape_pressed",e.EscapePressed}});
    bool releaseCancel=cancelOnRelease&&!abort&&!e.EscapePressed&&(e.KeyState&1)==0;
    if(abort||e.EscapePressed||releaseCancel)e.Action=DragAction.Cancel;
    else if((e.KeyState&1)==0)e.Action=DragAction.Drop;
    else e.Action=DragAction.Continue;
    base.OnQueryContinueDrag(e);
    terminalCancelOnRelease=releaseCancel&&e.Action==DragAction.Cancel;
    terminalKeyState=e.KeyState;terminalAction=e.Action.ToString();
  }
  protected override void OnFormClosed(FormClosedEventArgs e) {
    timer.Stop();timer.Dispose();
    if(ownedHwnd!=IntPtr.Zero&&IsWindow(ownedHwnd)&&GetProp(ownedHwnd,Property)==nonce)RemoveProp(ownedHwnd,Property);
    base.OnFormClosed(e);
  }
  public static void ShowSource(string dir,string[] paths,int nonce,bool closeCase,bool manualFocus) {
    IntPtr old=SetThreadDpiAwarenessContext(new IntPtr(-4));
    if(old==IntPtr.Zero)throw new InvalidOperationException("source per-monitor DPI context unavailable");
    try {Application.EnableVisualStyles();Application.SetCompatibleTextRenderingDefault(false);using(var form=new OwnedOleFileSource(dir,paths,nonce,closeCase,manualFocus))Application.Run(form);}
    finally {SetThreadDpiAwarenessContext(old);}
  }
}
'@
if($CompileOnly){
 if([string]::IsNullOrWhiteSpace($CompileReport) -or -not [IO.Path]::IsPathRooted($CompileReport)){throw 'Absolute compile report required'}
 $abi=[OwnedOleFileSource]::InputSourceAbiBytes();if($abi -ne 8){throw 'INPUT_MESSAGE_SOURCE ABI mismatch'}
 [OwnedOleFileSource]::Save($CompileReport,@{success=$true;compile_only=$true;window_created=$false;ole_started=$false;input_sent=$false;input_source_abi_bytes=$abi;pid=$PID;clr=[Environment]::Version.ToString();forms=[Windows.Forms.Form].Assembly.FullName})
 exit 0
}
if(-not $OleInputApproved){throw 'Separate physical OLE approval required; no window or input'}
if(-not [IO.Path]::IsPathRooted($Plan)){throw 'Absolute owned target plan required'}
$until=[DateTime]::UtcNow.AddSeconds(25)
while(-not (Test-Path -LiteralPath $Plan)){if([DateTime]::UtcNow -ge $until){throw 'Owned target plan timeout'};[Threading.Thread]::Sleep(10)}
$p=Get-Content -LiteralPath $Plan -Raw -Encoding utf8|ConvertFrom-Json
if($p.kind -ne 'owned-ole-target' -or $p.source_nonce -le 0 -or @($p.files).Count -ne 2 -or
    $p.close_while_dragging -isnot [bool] -or $p.close_while_dragging -ne [bool]$OleCloseWhileDragging -or
    $p.manual_source_focus -isnot [bool] -or $p.manual_source_focus -ne [bool]$OleManualSourceFocusApproved){throw 'Invalid owned source plan or focus/close case'}
$base=[IO.Path]::GetFullPath((Split-Path -Parent $Plan));$owned=[IO.Path]::GetFullPath($p.file_directory)
if([IO.Path]::GetFullPath((Split-Path -Parent $base)) -ne $owned){throw 'Plan files are not in exact owned case directory'}
$paths=@();foreach($f in $p.files){$path=[IO.Path]::GetFullPath($f.path);if((Split-Path -Parent $path) -ne $owned -or -not (Test-Path -LiteralPath $path -PathType Leaf)){throw 'Foreign source path rejected'};$paths+=([string]$path)}
[OwnedOleFileSource]::ShowSource($base,[string[]]$paths,[int]$p.source_nonce,[bool]$OleCloseWhileDragging,[bool]$OleManualSourceFocusApproved)
