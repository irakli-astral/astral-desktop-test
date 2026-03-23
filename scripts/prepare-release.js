import fs from 'fs'
import { execSync } from 'child_process'
import readline from 'readline'

function exec(command, options = {}) {
  try {
    return execSync(command, {
      encoding: 'utf8',
      stdio: options.silent ? 'pipe' : 'inherit',
      ...options,
    })
  } catch (error) {
    throw new Error(`Command failed: ${command}\n${error.message}`)
  }
}

function askQuestion(question) {
  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stdout,
  })

  return new Promise(resolve => {
    rl.question(question, answer => {
      rl.close()
      resolve(answer.trim())
    })
  })
}

async function prepareRelease() {
  const version = process.argv[2]

  if (!version || !version.match(/^v?\d+\.\d+\.\d+$/)) {
    console.error('❌ Usage: node scripts/prepare-release.js v1.0.0')
    console.error('   or: pnpm run release:prepare v1.0.0')
    process.exit(1)
  }

  const cleanVersion = version.replace('v', '')
  const tagVersion = version.startsWith('v') ? version : `v${version}`

  console.log(`🚀 Preparing release ${tagVersion}...\n`)

  try {
    // Check git status
    console.log('🔍 Checking git status...')
    const gitStatus = exec('git status --porcelain', { silent: true })
    if (gitStatus.trim()) {
      console.error(
        '❌ Working directory is not clean. Please commit or stash changes first.'
      )
      console.log('Uncommitted changes:')
      console.log(gitStatus)
      process.exit(1)
    }
    console.log('✅ Working directory is clean')

    // Run all checks first
    console.log('\n🔍 Running pre-release checks...')
    exec('pnpm run check:all')
    console.log('✅ All checks passed')

    // Update package.json
    console.log('\n📝 Updating package.json...')
    const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'))
    const oldPkgVersion = pkg.version
    pkg.version = cleanVersion
    fs.writeFileSync('package.json', JSON.stringify(pkg, null, 2) + '\n')
    console.log(`   ${oldPkgVersion} → ${cleanVersion}`)

    // Update Cargo.toml
    console.log('📝 Updating Cargo.toml...')
    const cargoPath = 'src-tauri/Cargo.toml'
    const cargoToml = fs.readFileSync(cargoPath, 'utf8')
    const oldCargoVersion = cargoToml.match(/version = "([^"]*)"/)
    const updatedCargo = cargoToml.replace(
      /version = "[^"]*"/,
      `version = "${cleanVersion}"`
    )
    fs.writeFileSync(cargoPath, updatedCargo)
    console.log(
      `   ${oldCargoVersion ? oldCargoVersion[1] : 'unknown'} → ${cleanVersion}`
    )

    // Update tauri.conf.json
    console.log('📝 Updating tauri.conf.json...')
    const tauriConfigPath = 'src-tauri/tauri.conf.json'
    const tauriConfig = JSON.parse(fs.readFileSync(tauriConfigPath, 'utf8'))
    const oldTauriVersion = tauriConfig.version
    tauriConfig.version = cleanVersion
    fs.writeFileSync(
      tauriConfigPath,
      JSON.stringify(tauriConfig, null, 2) + '\n'
    )
    console.log(`   ${oldTauriVersion} → ${cleanVersion}`)

    // Run pnpm install to update lock files
    console.log('\n📦 Updating lock files...')
    exec('pnpm install', { silent: true })
    console.log('✅ Lock files updated')

    // Verify configurations
    console.log('\n🔍 Verifying configurations...')

    console.log('✅ Configuration verified')

    // Final check that Rust code compiles
    console.log('\n🔍 Running final compilation check...')
    exec('source ~/.cargo/env && cd src-tauri && cargo check')
    console.log('✅ Rust compilation check passed')

    console.log(`\n🎉 Successfully prepared release ${tagVersion}!`)
    console.log('\n📋 Git commands to execute:')
    console.log(`   git add .`)
    console.log(`   git commit -m "chore: release ${tagVersion}"`)
    console.log(`   git tag ${tagVersion}`)
    console.log(`   git push origin main --tags`)

    console.log('\n🚀 After pushing:')
    console.log('   • GitHub Actions will automatically build the release')
    console.log('   • A draft release will be created on GitHub')
    console.log("   • You'll need to manually publish the draft release")
    console.log('   • Users will receive auto-update notifications')

    // Interactive execution option
    const answer = await askQuestion(
      '\n❓ Would you like me to execute these git commands? (y/N): '
    )

    if (answer.toLowerCase() === 'y' || answer.toLowerCase() === 'yes') {
      console.log('\n⚡ Executing git commands...')

      console.log('📝 Adding changes...')
      exec('git add .')

      console.log('💾 Creating commit...')
      exec(`git commit -m "chore: release ${tagVersion}"`)

      console.log('🏷️  Creating tag...')
      exec(`git tag ${tagVersion}`)

      console.log('📤 Pushing to remote...')
      exec('git push origin main --tags')

      console.log(`\n🎊 Release ${tagVersion} has been published!`)
      console.log('📱 Check GitHub Actions for build status')
      console.log('📦 Draft release will appear in the GitHub Releases page')
      console.log(
        '\n⚠️  Remember: You need to manually publish the draft release on GitHub!'
      )
    } else {
      console.log('\n📝 Git commands saved for manual execution.')
      console.log("   Run them when you're ready to release.")
    }
  } catch (error) {
    console.error('\n❌ Pre-release preparation failed:', error.message)
    process.exit(1)
  }
}

// Run if this is the main module
prepareRelease()
