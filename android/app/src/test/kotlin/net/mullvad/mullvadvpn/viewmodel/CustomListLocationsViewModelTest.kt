package net.mullvad.mullvadvpn.viewmodel

import app.cash.turbine.test
import arrow.core.right
import com.ramcosta.composedestinations.generated.navargs.toSavedStateHandle
import io.mockk.coEvery
import io.mockk.every
import io.mockk.mockk
import kotlin.test.assertIs
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.runTest
import net.mullvad.mullvadvpn.compose.communication.CustomListAction
import net.mullvad.mullvadvpn.compose.communication.CustomListActionResultData
import net.mullvad.mullvadvpn.compose.communication.LocationsChanged
import net.mullvad.mullvadvpn.compose.screen.CustomListLocationsNavArgs
import net.mullvad.mullvadvpn.compose.state.CustomListLocationsData
import net.mullvad.mullvadvpn.compose.state.CustomListLocationsUiState
import net.mullvad.mullvadvpn.lib.common.test.TestCoroutineRule
import net.mullvad.mullvadvpn.lib.common.test.assertLists
import net.mullvad.mullvadvpn.lib.model.CustomList
import net.mullvad.mullvadvpn.lib.model.CustomListId
import net.mullvad.mullvadvpn.lib.model.CustomListName
import net.mullvad.mullvadvpn.lib.model.GeoLocationId
import net.mullvad.mullvadvpn.lib.model.Ownership
import net.mullvad.mullvadvpn.lib.model.ProviderId
import net.mullvad.mullvadvpn.lib.model.RelayItem
import net.mullvad.mullvadvpn.lib.ui.component.relaylist.CheckableRelayListItem
import net.mullvad.mullvadvpn.relaylist.descendants
import net.mullvad.mullvadvpn.relaylist.withDescendants
import net.mullvad.mullvadvpn.repository.RelayListRepository
import net.mullvad.mullvadvpn.usecase.customlists.CustomListActionUseCase
import net.mullvad.mullvadvpn.usecase.customlists.CustomListRelayItemsUseCase
import net.mullvad.mullvadvpn.util.Lce
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.BeforeEach
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.extension.ExtendWith

@ExtendWith(TestCoroutineRule::class)
class CustomListLocationsViewModelTest {
    private val mockRelayListRepository: RelayListRepository = mockk()
    private val mockCustomListUseCase: CustomListActionUseCase = mockk()
    private val mockCustomListRelayItemsUseCase: CustomListRelayItemsUseCase = mockk()

    private val relayListFlow = MutableStateFlow<List<RelayItem.Location.Country>>(emptyList())
    private val selectedLocationsFlow = MutableStateFlow<List<RelayItem.Location>>(emptyList())

    @BeforeEach
    fun setup() {
        every { mockRelayListRepository.relayList } returns relayListFlow
        every { mockCustomListRelayItemsUseCase(any()) } returns selectedLocationsFlow
    }

    @Test
    fun `given new list false state uiState newList should be false`() = runTest {
        // Arrange
        val newList = false
        val customList =
            CustomList(
                id = CustomListId("id"),
                name = CustomListName.fromString("name"),
                locations = emptyList(),
            )
        relayListFlow.value = DUMMY_COUNTRIES
        val viewModel = createViewModel(customListId = customList.id, newList = newList)

        // Act, Assert
        viewModel.uiState.test {
            val state = awaitItem()
            assertEquals(newList, state.newList)
        }
    }

    @Test
    fun `when selected locations is not null and relay countries is not empty should return ui state content`() =
        runTest {
            // Arrange
            val expectedList =
                DUMMY_COUNTRIES.map {
                    CheckableRelayListItem(
                        item = it,
                        depth = it.toDepth(),
                        checked = false,
                        expanded = false,
                    )
                }
            val customListId = CustomListId("id")
            val expectedState =
                CustomListLocationsUiState(
                    newList = true,
                    Lce.Content(
                        CustomListLocationsData(
                            saveEnabled = false,
                            hasUnsavedChanges = false,
                            searchTerm = "",
                            locations = expectedList,
                        )
                    ),
                )
            val viewModel = createViewModel(customListId, true)
            relayListFlow.value = DUMMY_COUNTRIES

            // Act, Assert
            viewModel.uiState.test { assertEquals(expectedState, awaitItem()) }
        }

    @Test
    fun `when selecting parent should select children`() = runTest {
        // Arrange
        val expectedList = DUMMY_COUNTRIES
        val customListId = CustomListId("id")
        val expectedSelection = (DUMMY_COUNTRIES.take(1).withDescendants()).map { it.id }

        val viewModel = createViewModel(customListId, true)
        relayListFlow.value = expectedList

        // Act, Assert
        viewModel.uiState.test {
            // Check no selected
            val firstState = awaitItem()
            val firstStateContent = firstState.content
            assertIs<Lce.Content<CustomListLocationsData>>(firstStateContent)
            assertEquals(emptyList<RelayItem>(), firstStateContent.selectedLocations())
            // Expand country
            viewModel.onExpand(DUMMY_COUNTRIES[0], true)
            awaitItem()
            // Expand city
            viewModel.onExpand(DUMMY_COUNTRIES[0].cities[0], expand = true)
            awaitItem()
            // Select country
            viewModel.onRelaySelectionClick(DUMMY_COUNTRIES[0], true)
            // Check all items selected
            val secondState = awaitItem()
            val content = secondState.content
            assertIs<Lce.Content<CustomListLocationsData>>(content)
            assertLists(expectedSelection, content.selectedLocations())
        }
    }

    @Test
    fun `when deselecting child should deselect parent`() = runTest {
        // Arrange
        val expectedList = DUMMY_COUNTRIES
        val initialSelection = DUMMY_COUNTRIES.withDescendants()
        val initialSelectionIds = initialSelection.map { it.id }
        val customListId = CustomListId("id")
        val expectedSelection = emptyList<RelayItem>()
        relayListFlow.value = expectedList
        selectedLocationsFlow.value = initialSelection
        val viewModel = createViewModel(customListId, true)

        // Act, Assert
        viewModel.uiState.test {
            // Check initial selected
            val firstStateContent = awaitItem().content
            assertIs<Lce.Content<CustomListLocationsData>>(firstStateContent)
            assertEquals(initialSelectionIds, firstStateContent.selectedLocations())
            viewModel.onRelaySelectionClick(DUMMY_COUNTRIES[0].cities[0].relays[0], false)
            // Check all items selected
            val secondStateContent = awaitItem().content
            assertIs<Lce.Content<CustomListLocationsData>>(secondStateContent)
            assertEquals(expectedSelection, secondStateContent.selectedLocations())
        }
    }

    @Test
    fun `when deselecting parent should deselect child`() = runTest {
        // Arrange
        val expectedList = DUMMY_COUNTRIES
        val initialSelection =
            (DUMMY_COUNTRIES + DUMMY_COUNTRIES.flatMap { it.descendants() }).toSet()
        val initialSelectionIds = initialSelection.map { it.id }
        val customListId = CustomListId("id")
        val expectedSelection = emptyList<RelayItem>()
        relayListFlow.value = expectedList
        selectedLocationsFlow.value = initialSelection.toList()
        val viewModel = createViewModel(customListId, true)

        // Act, Assert
        viewModel.uiState.test {
            val firstStateContent = awaitItem().content
            assertIs<Lce.Content<CustomListLocationsData>>(firstStateContent)
            assertEquals(initialSelectionIds, firstStateContent.selectedLocations())
            viewModel.onRelaySelectionClick(DUMMY_COUNTRIES[0], false)
            // Check all items selected
            val secondStateContent = awaitItem().content
            assertIs<Lce.Content<CustomListLocationsData>>(secondStateContent)
            assertEquals(expectedSelection, secondStateContent.selectedLocations())
        }
    }

    @Test
    fun `when selecting child should not select parent`() = runTest {
        // Arrange
        val expectedList = DUMMY_COUNTRIES
        val customListId = CustomListId("id")
        val expectedSelection = DUMMY_COUNTRIES[0].cities[0].relays.map { it.id }
        val viewModel = createViewModel(customListId, true)
        relayListFlow.value = expectedList

        // Act, Assert
        viewModel.uiState.test {
            awaitItem() // Initial item
            // Expand country
            viewModel.onExpand(DUMMY_COUNTRIES[0], true)
            awaitItem()
            // Expand city
            viewModel.onExpand(DUMMY_COUNTRIES[0].cities[0], true)
            // Check no selected
            val firstStateContent = awaitItem().content
            assertIs<Lce.Content<CustomListLocationsData>>(firstStateContent)
            assertEquals(emptyList<RelayItem>(), firstStateContent.selectedLocations())
            viewModel.onRelaySelectionClick(DUMMY_COUNTRIES[0].cities[0].relays[0], true)
            // Check all items selected
            val secondStateContent = awaitItem().content
            assertIs<Lce.Content<CustomListLocationsData>>(secondStateContent)
            assertEquals(expectedSelection, secondStateContent.selectedLocations())
        }
    }

    @Test
    fun `given new list true when saving successfully should emit return with result data`() =
        runTest {
            // Arrange
            val customListId = CustomListId("1")
            val customListName = CustomListName.fromString("name")
            val newList = true
            val locationChangedMock: LocationsChanged = mockk()
            coEvery { mockCustomListUseCase(any<CustomListAction.UpdateLocations>()) } returns
                locationChangedMock.right()
            every { locationChangedMock.name } returns customListName
            every { locationChangedMock.id } returns customListId
            val viewModel = createViewModel(customListId, newList)

            // Act, Assert
            viewModel.uiSideEffect.test {
                viewModel.save()
                val sideEffect = awaitItem()
                assertIs<CustomListLocationsSideEffect.ReturnWithResultData>(sideEffect)
            }
        }

    @Test
    fun `given new list false when saving successfully should emit return with result data`() =
        runTest {
            // Arrange
            val customListId = CustomListId("1")
            val customListName = CustomListName.fromString("name")
            val mockUndo: CustomListAction.UpdateLocations = mockk()
            val addedLocations: List<GeoLocationId> = listOf(mockk())
            val removedLocations: List<GeoLocationId> = listOf(mockk())
            val newList = false
            val locationsChangedMock: LocationsChanged = mockk()
            val expectedResult =
                CustomListActionResultData.Success.LocationChanged(
                    customListName = customListName,
                    undo = mockUndo,
                )
            coEvery { mockCustomListUseCase(any<CustomListAction.UpdateLocations>()) } returns
                locationsChangedMock.right()
            every { locationsChangedMock.id } returns customListId
            every { locationsChangedMock.name } returns customListName
            every { locationsChangedMock.addedLocations } returns addedLocations
            every { locationsChangedMock.removedLocations } returns removedLocations
            every { locationsChangedMock.undo } returns mockUndo
            val viewModel = createViewModel(customListId, newList)

            // Act, Assert
            viewModel.uiSideEffect.test {
                viewModel.save()
                val sideEffect = awaitItem()
                assertIs<CustomListLocationsSideEffect.ReturnWithResultData>(sideEffect)
                assertEquals(expectedResult, sideEffect.result)
            }
        }

    @Test
    fun `given not new list and adding exactly one location and removing zero locations should emit location added`() =
        runTest {
            // Arrange
            val customListId = CustomListId("1")
            val customListName = CustomListName.fromString("name")
            val undo =
                CustomListAction.UpdateLocations(
                    id = customListId,
                    locations = listOf(DUMMY_COUNTRIES[0].id),
                )
            val expectedResult =
                CustomListActionResultData.Success.LocationAdded(
                    customListName = customListName,
                    locationName = DUMMY_RELAY.name,
                    undo = undo,
                )
            selectedLocationsFlow.value = DUMMY_COUNTRIES
            coEvery { mockCustomListUseCase(any<CustomListAction.UpdateLocations>()) } returns
                LocationsChanged(
                        id = customListId,
                        name = customListName,
                        locations = listOf(DUMMY_COUNTRIES[0].id, DUMMY_RELAY.id),
                        oldLocations = listOf(DUMMY_COUNTRIES[0].id),
                    )
                    .right()
            coEvery { mockRelayListRepository.find(DUMMY_RELAY.id) } returns DUMMY_RELAY

            val viewModel = createViewModel(customListId = customListId, newList = false)

            // Act, Assert
            viewModel.uiSideEffect.test {
                viewModel.onRelaySelectionClick(DUMMY_RELAY, true)
                viewModel.save()
                val sideEffect = awaitItem()
                assertIs<CustomListLocationsSideEffect.ReturnWithResultData>(sideEffect)
                assertEquals(expectedResult, sideEffect.result)
            }
        }

    private fun createViewModel(
        customListId: CustomListId,
        newList: Boolean,
    ): CustomListLocationsViewModel {
        return CustomListLocationsViewModel(
            relayListRepository = mockRelayListRepository,
            customListRelayItemsUseCase = mockCustomListRelayItemsUseCase,
            customListActionUseCase = mockCustomListUseCase,
            savedStateHandle =
                CustomListLocationsNavArgs(customListId = customListId, newList = newList)
                    .toSavedStateHandle(),
        )
    }

    private fun Lce.Content<CustomListLocationsData>.selectedLocations() =
        this.value.locations.filter { it.checked }.map { it.item.id }

    private fun RelayItem.Location.toDepth() =
        when (this) {
            is RelayItem.Location.Country -> 0
            is RelayItem.Location.City -> 1
            is RelayItem.Location.Relay -> 2
        }

    companion object {
        private val DUMMY_COUNTRIES =
            listOf(
                RelayItem.Location.Country(
                    name = "Sweden",
                    id = GeoLocationId.Country("SE"),
                    cities =
                        listOf(
                            RelayItem.Location.City(
                                name = "Gothenburg",
                                id = GeoLocationId.City(GeoLocationId.Country("SE"), "GBG"),
                                relays =
                                    listOf(
                                        RelayItem.Location.Relay(
                                            id =
                                                GeoLocationId.Hostname(
                                                    GeoLocationId.City(
                                                        GeoLocationId.Country("SE"),
                                                        "GBG",
                                                    ),
                                                    "gbg-1",
                                                ),
                                            active = true,
                                            provider = ProviderId("Provider"),
                                            ownership = Ownership.MullvadOwned,
                                            daita = false,
                                            quic = false,
                                        )
                                    ),
                            )
                        ),
                )
            )
        private val DUMMY_RELAY =
            RelayItem.Location.Relay(
                id =
                    GeoLocationId.Hostname(
                        GeoLocationId.City(GeoLocationId.Country("DK"), "CPH"),
                        "cph-1",
                    ),
                active = true,
                provider = ProviderId("Provider"),
                ownership = Ownership.MullvadOwned,
                daita = false,
                quic = false,
            )
    }
}
