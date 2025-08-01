@file:OptIn(ExperimentalSharedTransitionApi::class)

package net.mullvad.mullvadvpn.compose.screen

import android.os.Parcelable
import androidx.compose.animation.AnimatedVisibilityScope
import androidx.compose.animation.ExperimentalSharedTransitionApi
import androidx.compose.animation.SharedTransitionScope
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.tooling.preview.PreviewParameter
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.compose.dropUnlessResumed
import com.ramcosta.composedestinations.annotation.Destination
import com.ramcosta.composedestinations.annotation.RootGraph
import com.ramcosta.composedestinations.navigation.DestinationsNavigator
import kotlinx.parcelize.Parcelize
import net.mullvad.mullvadvpn.R
import net.mullvad.mullvadvpn.compose.cell.HeaderSwitchComposeCell
import net.mullvad.mullvadvpn.compose.cell.SwitchComposeSubtitleCell
import net.mullvad.mullvadvpn.compose.component.MullvadCircularProgressIndicatorLarge
import net.mullvad.mullvadvpn.compose.component.NavigateBackIconButton
import net.mullvad.mullvadvpn.compose.component.NavigateCloseIconButton
import net.mullvad.mullvadvpn.compose.component.ScaffoldWithMediumTopBar
import net.mullvad.mullvadvpn.compose.preview.MultihopUiStatePreviewParameterProvider
import net.mullvad.mullvadvpn.compose.transitions.SlideInFromRightTransition
import net.mullvad.mullvadvpn.lib.model.FeatureIndicator
import net.mullvad.mullvadvpn.lib.theme.AppTheme
import net.mullvad.mullvadvpn.lib.theme.Dimens
import net.mullvad.mullvadvpn.util.Lc
import net.mullvad.mullvadvpn.viewmodel.MultihopUiState
import net.mullvad.mullvadvpn.viewmodel.MultihopViewModel
import org.koin.androidx.compose.koinViewModel

@Preview("Loading|Enabled|Disabled")
@Composable
private fun PreviewMultihopScreen(
    @PreviewParameter(MultihopUiStatePreviewParameterProvider::class)
    state: Lc<Boolean, MultihopUiState>
) {
    AppTheme { MultihopScreen(state = state, onMultihopClick = {}, onBackClick = {}) }
}

@Parcelize data class MultihopNavArgs(val isModal: Boolean = false) : Parcelable

@OptIn(ExperimentalSharedTransitionApi::class)
@Destination<RootGraph>(style = SlideInFromRightTransition::class, navArgs = MultihopNavArgs::class)
@Composable
fun SharedTransitionScope.Multihop(
    animatedVisibilityScope: AnimatedVisibilityScope,
    navigator: DestinationsNavigator,
) {
    val viewModel = koinViewModel<MultihopViewModel>()
    val state by viewModel.uiState.collectAsStateWithLifecycle()

    MultihopScreen(
        state = state,
        modifier =
            Modifier.sharedBounds(
                rememberSharedContentState(key = FeatureIndicator.MULTIHOP),
                animatedVisibilityScope = animatedVisibilityScope,
            ),
        onMultihopClick = viewModel::setMultihop,
        onBackClick = dropUnlessResumed { navigator.navigateUp() },
    )
}

@Composable
fun MultihopScreen(
    state: Lc<Boolean, MultihopUiState>,
    onMultihopClick: (enable: Boolean) -> Unit,
    onBackClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    ScaffoldWithMediumTopBar(
        modifier = modifier,
        appBarTitle = stringResource(id = R.string.multihop),
        navigationIcon = {
            if (state.isModal()) {
                NavigateCloseIconButton(onBackClick)
            } else {
                NavigateBackIconButton(onNavigateBack = onBackClick)
            }
        },
    ) { modifier ->
        Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = modifier) {
            when (state) {
                is Lc.Loading -> Loading()
                is Lc.Content -> {
                    MultihopContent(state = state.value, onMultihopClick = onMultihopClick)
                }
            }
        }
    }
}

@Composable
private fun ColumnScope.MultihopContent(
    state: MultihopUiState,
    onMultihopClick: (enable: Boolean) -> Unit,
) {
    // Scale image to fit width up to certain width
    Image(
        contentScale = ContentScale.FillWidth,
        modifier =
            Modifier.widthIn(max = Dimens.settingsDetailsImageMaxWidth)
                .fillMaxWidth()
                .padding(horizontal = Dimens.mediumPadding)
                .align(Alignment.CenterHorizontally),
        painter = painterResource(id = R.drawable.multihop_illustration),
        contentDescription = stringResource(R.string.multihop),
    )
    Description()
    HeaderSwitchComposeCell(
        title = stringResource(R.string.enable),
        isToggled = state.enable,
        onCellClicked = onMultihopClick,
    )
}

@Composable
private fun Description() {
    SwitchComposeSubtitleCell(
        modifier = Modifier.padding(vertical = Dimens.mediumPadding),
        style = MaterialTheme.typography.labelLarge,
        text = stringResource(R.string.multihop_description),
    )
}

@Composable
private fun Loading() {
    MullvadCircularProgressIndicatorLarge()
}

private fun Lc<Boolean, MultihopUiState>.isModal(): Boolean =
    when (this) {
        is Lc.Loading -> this.value
        is Lc.Content -> this.value.isModal
    }
