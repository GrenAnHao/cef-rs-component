# Non-default validation only. Provision the exact fixed-Chromium English BDic
# in a fresh owned bundle; never install an OS language or write Custom Dictionary.
function Add-SpellingDictionaryFixture {
    param([Parameter(Mandatory)][string]$Dictionary,[Parameter(Mandatory)][string]$License,[Parameter(Mandatory)][string]$Bundle,[Parameter(Mandatory)][string]$Run)
    foreach($file in @($Dictionary,$License)){if(-not [IO.Path]::IsPathFullyQualified($file) -or -not (Test-Path -LiteralPath $file -PathType Leaf)){throw 'Spelling fixture requires explicit absolute dictionary/license files'}}
    $expected='a075b01d9b015c616511a9e87da77da3d9881621db32f584e4606ddabf1c1100'
    $hash=(Get-FileHash -LiteralPath $Dictionary -Algorithm SHA256).Hash.ToLowerInvariant()
    if($hash -ne $expected -or (Get-Item -LiteralPath $Dictionary).Length -ne 451968){throw 'Fixed en-US-10-1 BDic identity mismatch'}
    if((Get-Item -LiteralPath $License).Length -gt 65536 -or (Get-Item -LiteralPath $License).Length -eq 0){throw 'Spelling license budget'}
    $target=Join-Path $Bundle 'Dictionaries';if(Test-Path -LiteralPath $target){throw 'Refuse spelling fixture directory overwrite'}
    $manifestPath=Join-Path $Bundle 'component-runtime.json';$manifest=Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8|ConvertFrom-Json
    $before=Join-Path $Run 'component-runtime-before-spelling-fixture.json';if(Test-Path $before){throw 'Refuse before-fixture manifest overwrite'};Copy-Item -LiteralPath $manifestPath -Destination $before
    New-Item -ItemType Directory -Path $target|Out-Null
    Copy-Item -LiteralPath $Dictionary -Destination (Join-Path $target 'en-US-10-1.bdic');Copy-Item -LiteralPath $License -Destination (Join-Path $target 'README_en_US.txt')
    $copiedDictionary=Join-Path $target 'en-US-10-1.bdic'
    $copiedHash=(Get-FileHash -LiteralPath $copiedDictionary -Algorithm SHA256).Hash.ToLowerInvariant()
    if($copiedHash -ne $expected -or (Get-Item -LiteralPath $copiedDictionary).Length -ne 451968){throw 'Copied BDic identity mismatch'}
    $hash=$copiedHash
    $licenseHash=(Get-FileHash -LiteralPath (Join-Path $target 'README_en_US.txt')).Hash.ToLowerInvariant()
    if($licenseHash -ne '168b4c01cb841f766a72f310b065c04d09cff46376c37a81d799593ce751c371'){throw 'Fixed dictionary copyright/permission text identity mismatch'}
    $manifest.runtime=@($manifest.runtime)+@(@{path='Dictionaries/en-US-10-1.bdic';sha256=$hash},@{path='Dictionaries/README_en_US.txt';sha256=$licenseHash})
    Write-JsonFile $manifest $manifestPath
    Write-JsonFile @{dictionary=$Dictionary;license=$License;dictionary_sha256=$hash;dictionary_bytes=451968;license_sha256=$licenseHash;manifest_before_sha256=(Get-FileHash $before).Hash.ToLowerInvariant();manifest_after_sha256=(Get-FileHash $manifestPath).Hash.ToLowerInvariant();system_language_not_installed=$true;custom_dictionary_not_written=$true;fixture_only=$true} (Join-Path $Run 'spelling-dictionary-fixture.json')
}
